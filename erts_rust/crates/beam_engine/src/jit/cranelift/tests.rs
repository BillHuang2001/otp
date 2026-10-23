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
use crate::context::Mailbox;
use crate::jit::baseline::HALT;
use crate::{Machine, MinimalContext};
use beam_common::BeamError;
use beam_loader::{load_file, Instr, Operand};
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
        suspend: None,
        // No literal area is bound for this hand-built runtime block. The tier's
        // inline set never reads an `Operand::Literal` (a `move` with a literal
        // source is a shape the inline path declines and deopts to `jit_step_one`),
        // so a null area is inert here — exactly as the driver's own "null means
        // reject every access" discipline documents.
        literals_base: std::ptr::null(),
        literals_end: std::ptr::null(),
        resume_entry: std::ptr::null(),
        tables: crate::jit::baseline::runtime::EntryTables::NONE,
        compiled: std::ptr::null(),
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

// ---------------------------------------------------------------------------
// The retained-artifact store (`cache`) and the label map the artifact carries
// ---------------------------------------------------------------------------

/// The receive-family oracle fixture (`recv:wait/0` is `receive X -> X end`, so with an
/// empty mailbox it parks at the compiled `wait`).
const RECV_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/recv.beam"
);

/// A mailbox that never yields a candidate and never delivers one.
///
/// This is the honest host for a *parking* receive: `loop_rec` finds nothing, so the
/// fixture reaches its `wait` and the run suspends. A `send` has nowhere to go, which is
/// a typed refusal rather than a silently dropped message (no fixture here sends).
struct EmptyMailbox;

impl Mailbox for EmptyMailbox {
    fn send_message(&mut self, _to: Eterm, _message: Eterm) -> beam_common::Result<()> {
        Err(BeamError::BadArg)
    }

    fn receive_next(&mut self) -> beam_common::Result<Option<Eterm>> {
        Ok(None)
    }

    fn receive_skip(&mut self) -> beam_common::Result<()> {
        Ok(())
    }

    fn receive_remove(&mut self) -> beam_common::Result<()> {
        Ok(())
    }

    fn receive_reset(&mut self) -> beam_common::Result<()> {
        Ok(())
    }

    fn receive_marker_reserve(&mut self) -> beam_common::Result<Eterm> {
        Ok(beam_term::NIL)
    }

    fn receive_marker_bind(&mut self, _marker: Eterm, _reference: Eterm) -> beam_common::Result<()> {
        Ok(())
    }

    fn receive_marker_use(&mut self, _reference: Eterm) -> beam_common::Result<()> {
        Ok(())
    }

    fn receive_marker_clear(&mut self, _reference: Eterm) -> beam_common::Result<()> {
        Ok(())
    }
}

/// Run `slice:add/2` on `module` once, on the tier, from a fresh machine.
///
/// The module is **passed in** rather than loaded here: the store's identity includes the
/// atom-namespace witness, so two independently loaded copies of one `.beam` are two
/// entries by design (each carries its own atom table instance).
fn run_slice_add(module: &Module, a: i64, b: i64) -> RunOutcome {
    let add = module
        .atom_table
        .lookup("add")
        .expect("the fixture interns `add`");
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    run_outcome(
        &mut machine,
        module,
        add,
        2,
        &[make_small(a), make_small(b)],
        &mut ctx,
    )
    .expect("run slice:add/2 on the Cranelift tier")
}

#[test]
fn a_second_run_of_the_same_module_reuses_the_cached_artifact() {
    // Compile-once / run-many: the second run of one module must reuse the retained
    // artifact instead of recompiling the whole module.
    cache::clear();
    assert_eq!(cache::len(), 0, "the store starts empty");
    assert_eq!(cache::compile_count(), 0);

    let module = load_file(SLICE_BEAM).expect("load slice.beam");
    assert_eq!(
        run_slice_add(&module, 2, 3),
        RunOutcome::Returned(make_small(5))
    );
    assert_eq!(
        cache::compile_count(),
        1,
        "the first run compiles the module exactly once"
    );
    assert_eq!(cache::len(), 1, "and retains that one artifact");

    assert_eq!(
        run_slice_add(&module, 4, 6),
        RunOutcome::Returned(make_small(10))
    );
    assert_eq!(
        cache::compile_count(),
        1,
        "the second run of the same module must not recompile"
    );
    assert_eq!(cache::len(), 1, "and must not add a second entry");

    // The atom-namespace half of the key is load-bearing, not decoration: a separately
    // loaded copy of the very same `.beam` carries a different atom-table instance, so
    // the artifact compiled against the first namespace (with atom immediates baked into
    // its emitted `move`/`test`/`select_val` fast paths) must be a **miss**.
    let second_copy = load_file(SLICE_BEAM).expect("load slice.beam a second time");
    assert_eq!(
        run_slice_add(&second_copy, 2, 3),
        RunOutcome::Returned(make_small(5))
    );
    assert_eq!(
        cache::compile_count(),
        2,
        "a different atom namespace must not share the first artifact"
    );
    assert_eq!(cache::len(), 2);
}

#[test]
fn the_artifact_carries_the_module_wide_label_map() {
    // The artifact keeps the module-wide `Label -> (func, pc)` map it was compiled with,
    // so the driver resolves a run's entry label/imports against it (`RunLabels::Stored`)
    // rather than rebuilding one per run — and that map must agree with a freshly built
    // one on every label the module defines.
    let module = load_file(SLICE_BEAM).expect("load slice.beam");
    let compiled = CraneliftJit::new()
        .compile(&module)
        .expect("compile slice.beam");
    let rebuilt = crate::interpreter::build_label_map(&module);

    let add = module
        .atom_table
        .lookup("add")
        .expect("the fixture interns `add`");
    let entry_label = module.export(add, 2).expect("add/2 is exported");
    assert_eq!(
        crate::interpreter::resolve(compiled.labels(), entry_label).expect("the export resolves"),
        crate::interpreter::resolve(&rebuilt, entry_label).expect("the export resolves"),
        "the artifact's map must resolve `add/2` exactly as a rebuilt map does"
    );

    // Stronger: every label of every function resolves identically through both maps.
    for function in &module.functions {
        for &label in function.labels.keys() {
            let from_artifact = crate::interpreter::resolve(compiled.labels(), label);
            let from_rebuilt = crate::interpreter::resolve(&rebuilt, label);
            match (from_artifact, from_rebuilt) {
                (Ok(a), Ok(b)) => assert_eq!(
                    a, b,
                    "label {label:?} must resolve to the same (func, pc) through both maps"
                ),
                (Err(_), Err(_)) => {}
                (a, b) => panic!("label {label:?} resolved differently: {a:?} vs {b:?}"),
            }
        }
    }

    // The driver's seam therefore takes the `RunLabels::Stored` fast path for this
    // artifact (a `None` would make it rebuild the map through `Machine::label_map`).
    let code = CompiledEntries {
        compiled: &compiled,
    };
    assert!(
        code.labels(&module).is_some(),
        "NativeCode::labels must hand the driver the artifact's own map"
    );
}

#[test]
fn a_receive_park_reports_the_interpreters_address_and_keeps_the_artifact() {
    // A parked run is the shape the missing resume entry point exists for: the tier must
    // report the interpreter's own resume address, and the artifact it compiled must
    // still be retained afterwards (compiling it per run would leave nothing to resume
    // against).
    cache::clear();
    let module = load_file(RECV_BEAM).expect("load recv.beam");
    let wait = module
        .atom_table
        .lookup("wait")
        .expect("the fixture exports `wait/0`");

    // The interpreter's reference address for the same fixture and mailbox.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_mailbox(Box::new(EmptyMailbox));
    let interpreted = machine
        .run_export_outcome(&module, wait, 0, &[], &mut ctx)
        .expect("interpret recv:wait/0");
    let (func, pc) = match interpreted {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("recv:wait/0 must park in `receive`, got {other:?}"),
    };

    // The tier reports the very same address.
    let mut tiered_machine = Machine::new();
    let mut ctx = MinimalContext::with_mailbox(Box::new(EmptyMailbox));
    let tiered = run_outcome(&mut tiered_machine, &module, wait, 0, &[], &mut ctx)
        .expect("run recv:wait/0 on the Cranelift tier");
    assert_eq!(
        tiered,
        RunOutcome::Wait { func, pc },
        "the tier must report the interpreter's own resume address"
    );

    // `RunOutcome::Wait { func, pc }` *is* the address `Machine::resume_outcome` is handed;
    // resuming the interpreter's parked machine there re-examines the mailbox and parks at
    // the same address again (no message arrived), which pins that the reported address is
    // the resumption address and not merely a coincidental instruction index.
    let resumed = machine
        .resume_outcome(&module, func, pc, &mut ctx)
        .expect("resume the interpreter's park");
    assert_eq!(
        resumed,
        RunOutcome::Wait { func, pc },
        "the reported address must be the one a resume re-enters"
    );

    // The retention the resume needs: the artifact is still there after the park.
    assert_eq!(
        cache::len(),
        1,
        "the parked run's artifact must still be retained"
    );
    assert_eq!(cache::compile_count(), 1);
}

#[test]
fn a_synthetic_module_without_a_digest_is_not_cached() {
    // A hand-built module carries the all-zero `Module::default` checksum, i.e. no
    // load-time identity: the store refuses to invent one, so the module is compiled per
    // entry — always correct, merely a wasted recompile.
    cache::clear();
    let (module, f) = crate::jit::bench::support::Fixture::new("cranelift_cache_synthetic")
        .finish(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Move {
                    src: Operand::Integer(7),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        );
    assert_eq!(
        module.checksum(),
        [0u8; 16],
        "a hand-built module has no load-time digest"
    );

    for _ in 0..2 {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        assert_eq!(
            run_outcome(&mut machine, &module, f, 0, &[], &mut ctx).expect("run the fixture"),
            RunOutcome::Returned(make_small(7))
        );
    }
    assert_eq!(cache::len(), 0, "a synthetic module is never retained");
    assert_eq!(
        cache::compile_count(),
        0,
        "and no compilation of it is recorded"
    );
}

#[test]
fn clear_drops_every_artifact_and_resets_the_compile_count() {
    cache::clear();
    // Two distinct real modules, so the store holds two entries.
    let slice = load_file(SLICE_BEAM).expect("load slice.beam");
    assert_eq!(
        run_slice_add(&slice, 1, 2),
        RunOutcome::Returned(make_small(3))
    );
    let coverage = load_file(COVERAGE_BEAM).expect("load coverage.beam");
    let other = coverage
        .atom_table
        .lookup("other")
        .expect("the fixture interns `other`");
    let classify = coverage
        .atom_table
        .lookup("classify")
        .expect("the fixture interns `classify`");
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    assert_eq!(
        run_outcome(
            &mut machine,
            &coverage,
            classify,
            1,
            &[beam_term::make_atom(other)],
            &mut ctx
        )
        .expect("run coverage:classify/1"),
        RunOutcome::Returned(beam_term::make_atom(other))
    );
    assert_eq!(cache::len(), 2, "two distinct modules are two entries");
    assert_eq!(cache::compile_count(), 2, "one compilation each");

    cache::clear();
    assert_eq!(cache::len(), 0, "clear drops every artifact");
    assert_eq!(cache::compile_count(), 0, "clear resets the compile counter");

    // The artifact was really released, not merely hidden: the next run compiles again.
    assert_eq!(
        run_slice_add(&slice, 5, 5),
        RunOutcome::Returned(make_small(10))
    );
    assert_eq!(cache::len(), 1);
    assert_eq!(cache::compile_count(), 1);
}