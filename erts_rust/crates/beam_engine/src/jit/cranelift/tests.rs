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
use crate::jit::Tier;
use crate::{Machine, MinimalContext};
use crate::jit::baseline::HALT;
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
fn a_synthetic_module_is_cached_under_its_content_fingerprint() {
    // A hand-built module carries the all-zero `Module::default` checksum, i.e. no
    // load-time digest, so the store identifies it by the crate's shared `CacheKey` —
    // whose module half is then the **content fingerprint** of the decoded IR. The
    // consequence is the baseline cache's: the first entry compiles, a second entry of
    // the very same module reuses that artifact (`CacheKey::of` is a pure function of
    // the module's content and atom namespace), and a module that *differs* in any
    // content still misses because the fingerprint differs.
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
    assert_eq!(
        cache::len(),
        1,
        "the digest-less module is retained under its content fingerprint"
    );
    assert_eq!(
        cache::compile_count(),
        1,
        "so the second entry of the same module recompiles nothing"
    );

    // The fingerprint is content, not name: the module half of the shared key is a hash
    // of the decoded IR, so the same module whose *body* changed keys differently — a
    // stale artifact can never be handed back for it. (A `Module::clone` carries a fresh
    // atom-table instance by design, so only the module half is compared here.)
    let before = crate::jit::cache::CacheKey::of(&module).module;
    let mut edited = module.clone();
    edited.functions[0].code[1] = Instr::Move {
        src: Operand::Integer(8),
        dst: Operand::X(0),
    };
    assert_ne!(
        before,
        crate::jit::cache::CacheKey::of(&edited).module,
        "a changed body must change the digest-less module's key"
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

// ---------------------------------------------------------------------------
// the native register-write extent (`CompiledEntries::enter`)
// ---------------------------------------------------------------------------

#[test]
fn a_native_entry_declares_the_register_write_extent() {
    // The register-write half of the tier's soundness contract. Inlined code stores
    // straight into `machine.x` through the pointer it holds, which no `set_x` mark can
    // observe, so a native entry must declare the artifact's own write extent
    // (`highest_native_register + 1`) **before** the call: that figure bounds the next
    // `Machine::reset`'s refill (a stale register above it would be a wrong result) and
    // lets a helper called mid-body hand a collecting host a root prefix covering those
    // stores.
    //
    // The observation is made with **no interpreter step in between**: the runtime block
    // is built by hand and the compiled entry is called directly, so the extent right
    // after the call can only be the tier's own note (a `set_x` high-water mark would need
    // a `set_x`, and the wide fallback would read 1024).
    const DEEP: u32 = 9;
    let (module, f) = crate::jit::bench::support::Fixture::new("cranelift_native_extent")
        .finish(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Move {
                    src: Operand::Integer(7),
                    dst: Operand::X(DEEP),
                },
                Instr::Return,
            ],
        );
    let entry_label = module.export(f, 0).expect("f/0 is exported");
    let labels = crate::interpreter::build_label_map(&module);
    let (func, pc) =
        crate::interpreter::resolve_label(&module, entry_label).expect("the entry label resolves");

    let compiled = CraneliftJit::new()
        .compile(&module)
        .expect("compile the fixture");
    assert_eq!(
        compiled.highest_native_register(),
        Some(DEEP),
        "the deep `move` is lowered natively, so the artifact bounds it"
    );
    let code = CompiledEntries {
        compiled: &compiled,
    };

    let mut machine = Machine::new();
    machine.reset();
    machine.push_frame(0).expect("push the bottom frame");
    machine.set_literals(module.literals.clone());
    assert_eq!(
        machine.used_x_extent(),
        0,
        "a fresh machine has no used x-register slot, so nothing but the tier's own note \
         can raise the extent"
    );

    let mut ctx = MinimalContext::new();
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
        literals_base: std::ptr::null(),
        literals_end: std::ptr::null(),
        resume_entry: std::ptr::null(),
        tables: crate::jit::baseline::runtime::EntryTables::NONE,
        compiled: std::ptr::null(),
    };

    let word = code
        .enter(&mut rt, &module, func, pc)
        .expect("the entry address is compiled");
    assert_eq!(word, HALT);
    // SAFETY: `machine_ptr` points at the live `machine` above; no native code is
    // running here.
    let (deep_slot, extent) = unsafe { ((*machine_ptr).x(DEEP), (*machine_ptr).used_x_extent()) };
    assert_eq!(
        deep_slot,
        Some(make_small(7)),
        "the inlined `move` wrote the deep register natively"
    );
    assert_eq!(
        extent,
        DEEP + 1,
        "the entry must declare one past the highest x slot the artifact addresses — \
         without that note the native store is invisible to the extent, the next reset \
         leaves it behind and a later run hands a stale word to a host as a live root"
    );

    machine.reset();
    assert_eq!(
        machine.x(DEEP),
        Some(beam_term::NIL),
        "a deep native write must not survive the reset"
    );
    assert!(machine.x.iter().all(|slot| *slot == beam_term::NIL));
    assert!(
        machine.fr.iter().all(|slot| *slot == beam_term::NIL),
        "nothing in this tier stores into the FR file, so no FR slot is dirty"
    );
    assert_eq!(
        machine.refilled_words(),
        DEEP + 1,
        "the refill is exactly the declared x extent (and no FR word), not the 2048 words \
         of the two register files"
    );
}

#[test]
fn a_driver_run_on_a_real_module_declares_the_extent_too() {
    // The same declaration on the driver path — the `NativeCode` seam the shared native
    // driver calls — over a real `.beam`: after the run the machine's used x extent covers
    // every register the artifact addresses natively, and the reset that follows wipes all
    // of them.
    let module = load_file(SLICE_BEAM).expect("load slice.beam");
    let add = module
        .atom_table
        .lookup("add")
        .expect("the fixture interns `add`");
    let compiled = CraneliftJit::new()
        .compile(&module)
        .expect("compile slice.beam");
    let highest = compiled
        .highest_native_register()
        .expect("the fixture inlines at least one register-addressed instruction");
    let code = CompiledEntries {
        compiled: &compiled,
    };

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let outcome = run_native_outcome(
        &mut machine,
        &module,
        add,
        2,
        &[make_small(2), make_small(3)],
        &mut ctx,
        &code,
    )
    .expect("run slice:add/2 natively");
    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(5)),
        "the declared extent changes no result"
    );
    assert!(
        machine.used_x_extent() >= highest + 1,
        "the driver's native entry must declare the artifact's write extent (up to x{highest}); \
         got an extent of {}",
        machine.used_x_extent()
    );

    machine.reset();
    assert!(
        machine.x.iter().all(|slot| *slot == beam_term::NIL),
        "the refill covers every register the artifact can address natively"
    );
}

/// A context that records the x-register root slice it is handed at the **heap admission
/// point** (`ensure_heap_full`, which `Machine::ensure_heap` reaches for a `test_heap`),
/// delegating the heap itself to a [`MinimalContext`].
///
/// The Cranelift twin of the baseline tier's `RootProbeCtx` — this tier caches no
/// registers, so what the probe checks is the *extent* the tier declared before entering
/// the block, not a spill.
struct RootProbeCtx {
    inner: MinimalContext,
    /// Every handed x slice, in call order.
    x_slices: Vec<Vec<Eterm>>,
    /// The `live` hint of each of those calls.
    lives: Vec<u32>,
}

impl RootProbeCtx {
    fn new() -> RootProbeCtx {
        RootProbeCtx {
            inner: MinimalContext::new(),
            x_slices: Vec::new(),
            lives: Vec::new(),
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
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    /// The recording door. The narrow `ensure_heap` above records nothing, so a run that
    /// reached only it leaves `x_slices` empty and the assertions below fail loudly.
    fn ensure_heap_full(
        &mut self,
        heap_need: u32,
        roots: crate::context::RootSet<'_>,
    ) -> Result<()> {
        self.lives.push(roots.live);
        self.x_slices.push(roots.x.to_vec());
        let live = roots.live;
        self.inner
            .ensure_heap(heap_need, live, roots.x, roots.stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.inner.alloc_words(n)
    }
}

#[test]
fn the_declared_extent_lets_a_mid_block_helper_see_native_writes() {
    // The root-set half of the same contract: a helper the compiled body calls **in the
    // middle of itself** reads the register files through the same extent, so an inlined
    // store earlier in the same native function must already be inside the prefix the host
    // is handed. Without the pre-call note the host would see a zero-length slice — a
    // missed relocation, i.e. a stale pointer after a moving collection.
    //
    // The experiment is a body that inlines two `move`s (one into the deep `x9`) and then
    // reaches `test_heap`, which this tier deopts to `jit_step_one` — the interpreter's own
    // step, hence `Machine::ensure_heap` → `with_roots` → the probe.
    const DEEP: u32 = 9;
    let (module, f) = crate::jit::bench::support::Fixture::new("cranelift_extent_probe")
        .finish(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Move {
                    src: Operand::Integer(11),
                    dst: Operand::X(0),
                },
                Instr::Move {
                    src: Operand::Integer(22),
                    dst: Operand::X(DEEP),
                },
                Instr::TestHeap {
                    heap_need: beam_loader::HeapNeed::Words(8),
                    live: DEEP + 1,
                },
                Instr::Return,
            ],
        );
    let compiled = CraneliftJit::new()
        .compile(&module)
        .expect("compile the fixture");
    assert_eq!(
        compiled.highest_native_register(),
        Some(DEEP),
        "both `move`s are lowered natively, not deopted"
    );
    let code = CompiledEntries {
        compiled: &compiled,
    };

    let mut machine = Machine::new();
    let mut probe = RootProbeCtx::new();
    let outcome = run_native_outcome(&mut machine, &module, f, 0, &[], &mut probe, &code)
        .expect("the natively compiled run returns");
    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(11)),
        "the fixture still returns x(0) — the extent is a root-set figure, never a value"
    );

    assert_eq!(
        probe.x_slices.len(),
        1,
        "exactly one heap admission point was reached"
    );
    assert_eq!(
        probe.lives,
        vec![DEEP + 1],
        "the instruction's own live count is forwarded unchanged"
    );
    let handed = &probe.x_slices[0];
    assert_eq!(
        handed.len() as u32,
        DEEP + 1,
        "the hand-off is the artifact's own x write extent, declared before the native call \
         — without it the inlined stores are invisible and the host would be handed an \
         empty slice"
    );
    assert_eq!(
        handed[0],
        make_small(11),
        "x0 must hold its moved value at the helper boundary"
    );
    assert_eq!(
        handed[DEEP as usize],
        make_small(22),
        "the deep inlined write must be inside the handed root prefix"
    );
}
// ---------------------------------------------------------------------------
// The resume entry point (`run_resume_outcome`)
// ---------------------------------------------------------------------------
//
// A parked machine is handed back to native code, so every case here is a park /
// resume pair compared **clause for clause** with the interpreter: the same
// `RunOutcome`, the same value, the same typed errors. The interpreter side always
// goes through [`Machine::resume_outcome`], the tier under test through
// [`Machine::resume_outcome_with_tier`] (`Tier::Cranelift`), which is the public door
// that reaches `run_resume_outcome` → `baseline::run_native_resume_outcome`.

/// Assemble several IR functions into one module, the first one exported.
///
/// [`crate::jit::bench::support::Fixture`] builds a **one**-function module; the resume
/// cases here need a second function so the parked address can sit on the far side of a
/// cross-function transfer (which is where the native driver regains control from a
/// compiled block). Each function's blocks come from the loader's own decomposition, so a
/// compiled function never lacks an entry point.
fn multi_function_module(
    name: &str,
    imports: &[(&str, &str, u32)],
    functions: Vec<(&str, u32, Vec<Instr>)>,
) -> (Module, Atom) {
    let mut module = Module::default();
    module.name = module.atom_table.intern(name);
    for (import_module, import_function, arity) in imports {
        let m = module.atom_table.intern(import_module);
        let f = module.atom_table.intern(import_function);
        module.imports.push(beam_loader::Import {
            module: m,
            function: f,
            arity: *arity,
        });
    }
    let mut first = None;
    for (function_name, arity, code) in functions {
        let function = module.atom_table.intern(function_name);
        let (blocks, labels) = beam_loader::lower::build_blocks(&code);
        let entry = blocks.first().map_or(Label(0), |block| block.label);
        module.functions.push(beam_loader::Function {
            func_label: entry,
            entry_label: entry,
            module: module.name,
            name: function,
            arity,
            code,
            blocks,
            labels,
        });
        if first.is_none() {
            first = Some((function, arity, entry));
        }
    }
    let (function, arity, label) = first.expect("at least one function");
    module.exports.push(beam_loader::Export {
        function,
        arity,
        label,
    });
    (module, function)
}

/// The park-and-resume fixture: `run/0` calls `g/0` (a **cross-function** transfer, so the
/// native driver regains control between them), then adds `1` to `g`'s result with the
/// inlined `erlang:'+'/2` fast path.
///
/// ```text
/// run/0:                          g/0:
///   pc0 label 1                     pc0 label 10
///   pc1 call 0 label 10             pc1 move 7 -> x0
///   pc2 move 1 -> x1                pc2 return
///   pc3 gc_bif + x0 x1 -> x0
///   pc4 return
/// ```
///
/// Uninterrupted it answers `8` in three driver iterations on the native tier (one per
/// function entry/return) and in eight instructions on the interpreter.
fn park_fixture() -> (Module, Atom) {
    let run = vec![
        Instr::Label(Label(1)),
        Instr::Call {
            arity: 0,
            target: Label(10),
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(1),
        },
        Instr::GcBif {
            arity: 2,
            bif: beam_loader::ImportRef(0),
            fail: beam_loader::NO_FAIL,
            live: 2,
            args: vec![Operand::X(0), Operand::X(1)],
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let callee = vec![
        Instr::Label(Label(10)),
        Instr::Move {
            src: Operand::Integer(7),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    multi_function_module(
        "cranelift_park",
        &[("erlang", "+", 2)],
        vec![("run", 0, run), ("g", 0, callee)],
    )
}

/// A context that wraps [`MinimalContext`] (borrowing its pointer-stable heap) and
/// pre-empts through the **reduction hook**: the charge past `budget` is a typed `Err`,
/// which parks the machine at the address about to run, with nothing of it executed.
///
/// `wants_reduce()` is forced `true` — `MinimalContext` itself opts out, so the engine
/// would otherwise never call the hook.
struct ChargeCtx {
    inner: MinimalContext,
    /// The `n` of every `reduce` call, in order (the refusing one included).
    charges: Vec<u32>,
    /// `None` once the budget is lifted, so the resumed slice runs to completion.
    budget: Option<u32>,
}

impl ChargeCtx {
    fn with_budget(budget: u32) -> ChargeCtx {
        ChargeCtx {
            inner: MinimalContext::with_bif_hook(crate::jit::bench::support::arith_hook(
                None, None,
            )),
            charges: Vec::new(),
            budget: Some(budget),
        }
    }

    /// Lift the budget (the resumed slice is unbounded).
    fn lift(&mut self) {
        self.budget = None;
    }
}

impl ExecutionContext for ChargeCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.inner.call_bif(module, function, arity, args)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.inner.alloc_words(n)
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

    fn reduce(&mut self, n: u32) -> Result<()> {
        self.charges.push(n);
        match self.budget {
            Some(budget) if self.charges.len() as u32 > budget => {
                Err(crate::error::unsupported("reduction budget exhausted"))
            }
            _ => Ok(()),
        }
    }

    fn wants_reduce(&self) -> bool {
        true
    }
}

/// A mailbox whose queue a test can fill **after** the park (the park is only meaningful
/// while no message is waiting).
#[derive(Clone, Default)]
struct SharedMessages(std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<Eterm>>>);

impl SharedMessages {
    fn push(&self, message: Eterm) {
        self.0.borrow_mut().push_back(message);
    }
}

struct ScriptedMailbox {
    messages: SharedMessages,
}

impl Mailbox for ScriptedMailbox {
    fn send_message(&mut self, _to: Eterm, message: Eterm) -> beam_common::Result<()> {
        self.messages.push(message);
        Ok(())
    }

    fn receive_next(&mut self) -> beam_common::Result<Option<Eterm>> {
        Ok(self.messages.0.borrow_mut().pop_front())
    }

    fn receive_skip(&mut self) -> beam_common::Result<()> {
        Ok(())
    }

    fn receive_remove(&mut self) -> beam_common::Result<()> {
        // The consumed candidate is the one `receive_next` handed out.
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

#[test]
fn a_reduction_refusal_parks_the_machine_and_the_tier_resumes_it() {
    // Case 1 of the resume contract: a pre-empting host (`reduce` answers a typed `Err`)
    // parks the machine at the address about to run, and the tier resumes there — the
    // registers, the Erlang stack and the live call frame intact — finishing with the
    // interpreter's own value.
    cache::clear();
    let (module, run) = park_fixture();

    // The uninterrupted runs are the reference: the tier and the interpreter agree, and
    // the tier really did compile (a synthetic module is cached on its content
    // fingerprint, so that is observable).
    let mut tiered = Machine::new();
    let mut tiered_ctx = ChargeCtx::with_budget(u32::MAX);
    assert_eq!(
        tiered
            .run_export_outcome_with_tier(&module, run, 0, &[], &mut tiered_ctx, Tier::Cranelift)
            .expect("the uninterrupted cranelift run returns"),
        RunOutcome::Returned(make_small(8))
    );
    assert_eq!(
        cache::compile_count(),
        1,
        "the tier compiled the fixture instead of interpreting it"
    );
    let tiered_uninterrupted_charges = tiered_ctx.charges.len();

    let mut interpreter = Machine::new();
    let mut interpreter_ctx = ChargeCtx::with_budget(u32::MAX);
    assert_eq!(
        interpreter
            .run_export_outcome_with_tier(
                &module,
                run,
                0,
                &[],
                &mut interpreter_ctx,
                Tier::Interpreter,
            )
            .expect("the uninterrupted interpreter run returns"),
        RunOutcome::Returned(make_small(8))
    );
    let interpreter_uninterrupted_charges = interpreter_ctx.charges.len();
    assert!(
        interpreter_uninterrupted_charges > tiered_uninterrupted_charges,
        "the interpreter is charged per instruction ({}), the native driver per block entry \
         ({tiered_uninterrupted_charges})",
        interpreter_uninterrupted_charges
    );

    // Park both on the same budget: one charge of headroom.
    let mut parked = Machine::new();
    let mut parked_ctx = ChargeCtx::with_budget(1);
    parked
        .run_export_outcome_with_tier(&module, run, 0, &[], &mut parked_ctx, Tier::Cranelift)
        .expect_err("the second charge is refused");

    let mut parked_interpreter = Machine::new();
    let mut parked_interpreter_ctx = ChargeCtx::with_budget(1);
    parked_interpreter
        .run_export_outcome_with_tier(&module, run, 0, &[], &mut parked_interpreter_ctx, Tier::Interpreter)
        .expect_err("the second charge is refused");

    // The park address is the address about to run, never an already-executed one.
    let (func, pc) = parked.current_code().expect("the driver published its position");
    assert_eq!(
        (func, pc),
        (1, 0),
        "the tier parked on the callee's entry, with nothing of it executed"
    );
    let (ifunc, ipc) = parked_interpreter
        .current_code()
        .expect("the interpreter published its position");
    assert_eq!(
        (ifunc, ipc),
        (0, 1),
        "the interpreter parked on the instruction it refused to charge"
    );

    // Resume both, with a message-free budget (the budget is lifted, nothing else changes).
    let charges_before = parked_ctx.charges.len();
    parked_ctx.lift();
    assert_eq!(
        parked
            .resume_outcome_with_tier(&module, func, pc, &mut parked_ctx, Tier::Cranelift)
            .expect("the cranelift tier resumes the parked process"),
        RunOutcome::Returned(make_small(8)),
        "a resumed cranelift slice must produce the interpreter's value"
    );
    parked_interpreter_ctx.lift();
    assert_eq!(
        parked_interpreter
            .resume_outcome_with_tier(&module, ifunc, ipc, &mut parked_interpreter_ctx, Tier::Interpreter)
            .expect("the interpreter resumes the parked process"),
        RunOutcome::Returned(make_small(8))
    );

    // Nothing was replayed: the total charge count across park + resume is the
    // uninterrupted count plus exactly the one refused charge (the resumed slice
    // re-charges the block/instruction the park left unexecuted). A replay of the
    // completed prefix would show up as a larger total.
    assert_eq!(
        parked_ctx.charges.len(),
        tiered_uninterrupted_charges + 1,
        "the resume re-charges only the parked address, never the executed prefix"
    );
    assert_eq!(
        parked_interpreter_ctx.charges.len(),
        interpreter_uninterrupted_charges + 1
    );

    // And the slice counter is per slice: the resumed slice charged exactly the calls made
    // in it (the counter is not carried across the park, so nothing is double-counted).
    let resumed_slice_charges = (parked_ctx.charges.len() - charges_before) as u64;
    assert_eq!(
        parked.reductions(),
        resumed_slice_charges,
        "the resumed slice's reduction count is its own charges, not the run's"
    );
}

#[test]
fn a_receive_park_on_the_tier_resumes_with_the_interpreters_result() {
    // Case 2 of the resume contract: a `receive` that finds nothing parks the machine at
    // the interpreter's own address, and the resume — once a message arrives — completes
    // with the identical result.
    cache::clear();
    let module = load_file(RECV_BEAM).expect("load recv.beam");
    let wait = module
        .atom_table
        .lookup("wait")
        .expect("the fixture exports `wait/0`");

    // Interpreter reference: park, then resume with the message.
    let mut interpreter = Machine::new();
    let messages = SharedMessages::default();
    let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
        messages: messages.clone(),
    }));
    let interpreted_park = interpreter
        .run_export_outcome_with_tier(&module, wait, 0, &[], &mut ctx, Tier::Interpreter)
        .expect("interpret recv:wait/0");
    let (func, pc) = match interpreted_park {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("recv:wait/0 must park in `receive`, got {other:?}"),
    };
    messages.push(make_small(42));
    let expected = interpreter
        .resume_outcome(&module, func, pc, &mut ctx)
        .expect("the interpreter resume completes");
    assert_eq!(expected, RunOutcome::Returned(make_small(42)));

    // The tier: the very same park address, then the identical resumed result.
    let mut machine = Machine::new();
    let tier_messages = SharedMessages::default();
    let mut tier_ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
        messages: tier_messages.clone(),
    }));
    assert_eq!(
        machine
            .run_export_outcome_with_tier(&module, wait, 0, &[], &mut tier_ctx, Tier::Cranelift)
            .expect("run recv:wait/0 on the Cranelift tier"),
        RunOutcome::Wait { func, pc },
        "the tier must report the interpreter's own resume address"
    );
    tier_messages.push(make_small(42));
    assert_eq!(
        machine
            .resume_outcome_with_tier(&module, func, pc, &mut tier_ctx, Tier::Cranelift)
            .expect("the cranelift tier resumes the parked process"),
        expected,
        "a resumed cranelift slice must agree with the interpreter exactly"
    );
    assert_eq!(
        cache::compile_count(),
        1,
        "the resume reused the artifact the park compiled"
    );
}

#[test]
fn a_resume_at_the_reported_wait_pc_advances_by_the_interpreters_rule() {
    // Case 3 of the resume contract: a resume address the compiled function cannot be
    // entered at is **not** an error — the driver takes its per-instruction interpreter
    // step there and the run completes with the interpreter's result.
    //
    // A `receive` park reports the `wait` instruction's **branch target** (the
    // `loop_rec`), which is a block boundary and therefore typically enterable; the
    // non-enterable arm of the same rule is reached by every address `is_enterable`
    // rejects (a mid-block pc, and every function the tier holds no compiled blocks
    // for — the cross-module callee of the next case). What the resume must guarantee
    // either way is the interpreter's own outcome, so this case pins the park address
    // and the result against `Tier::Interpreter`.
    cache::clear();
    let module = load_file(RECV_BEAM).expect("load recv.beam");
    let wait = module
        .atom_table
        .lookup("wait")
        .expect("the fixture exports `wait/0`");

    // Interpreter reference: park, then resume with the same message.
    let reference_messages = SharedMessages::default();
    let mut reference = Machine::new();
    let mut reference_ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
        messages: reference_messages.clone(),
    }));
    let (ifunc, ipc) = match reference
        .run_export_outcome_with_tier(&module, wait, 0, &[], &mut reference_ctx, Tier::Interpreter)
        .expect("interpret recv:wait/0")
    {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("recv:wait/0 must park in `receive`, got {other:?}"),
    };
    reference_messages.push(make_small(7));
    let expected = reference
        .resume_outcome(&module, ifunc, ipc, &mut reference_ctx)
        .expect("the interpreter resume completes");

    let messages = SharedMessages::default();
    let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
        messages: messages.clone(),
    }));
    let mut machine = Machine::new();
    let (func, pc) = match machine
        .run_export_outcome_with_tier(&module, wait, 0, &[], &mut ctx, Tier::Cranelift)
        .expect("run recv:wait/0 on the Cranelift tier")
    {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("recv:wait/0 must park in `receive`, got {other:?}"),
    };
    assert_eq!(
        (func, pc),
        (ifunc, ipc),
        "the tier must report the interpreter's park address"
    );

    // The artifact this thread compiled is retained across the park, and it names the
    // function entry as an entry point (so the resume may re-enter natively).
    let compiled = cache::handle(&module).expect("the parked run's artifact is retained");
    assert!(
        compiled.is_enterable(func, 0),
        "the function's entry pc is a native entry point"
    );

    messages.push(make_small(7));
    assert_eq!(
        machine
            .resume_outcome_with_tier(&module, func, pc, &mut ctx, Tier::Cranelift)
            .expect("a resume at a non-enterable address is a per-instruction step, not an error"),
        expected,
        "the resumed cranelift slice must agree with the interpreter"
    );
    assert_eq!(expected, RunOutcome::Returned(make_small(7)));
}

#[test]
fn a_resume_parked_in_a_non_zero_module_context_agrees_with_the_interpreter() {
    // Case 4 of the resume contract: the process parked **inside a cross-module callee**,
    // so the machine's current module context is not `0`. The driver lands in that context
    // (`Machine::current_context` → the context switch) and re-enters the callee's
    // `(func, pc)`; because the Cranelift tier holds no compiled blocks for that callee
    // (`NativeCode::module_blocks` defaults to `None`), the callee runs on the
    // per-instruction interpreter step there — correct, not an error.
    cache::clear();

    // The callee `c:g/1`: a bare `receive` that parks, then returns the matched message.
    let (callee_module, g) = multi_function_module(
        "c",
        &[],
        vec![(
            "g",
            1,
            vec![
                Instr::Label(Label(10)),
                Instr::LoopRec {
                    fail: Label(11),
                    dst: Operand::X(0),
                },
                Instr::RemoveMessage,
                Instr::Return,
                Instr::Label(Label(11)),
                Instr::Wait { target: Label(10) },
            ],
        )],
    );
    let callee = std::sync::Arc::new(callee_module);
    let callee_entry = callee.export(g, 1).expect("c:g/1 is exported");

    // The caller `run/0` applies `c:g/1` through the `resolve_external_call` seam.
    let (caller, run) = {
        let mut module = Module::default();
        module.name = module.atom_table.intern("caller");
        let c = module.atom_table.intern("c");
        let gg = module.atom_table.intern("g");
        let run_name = module.atom_table.intern("run");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::Atom(c),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Atom(gg),
                dst: Operand::X(2),
            },
            Instr::Apply { arity: 1 },
            Instr::Return,
        ];
        let (blocks, labels) = beam_loader::lower::build_blocks(&code);
        let entry = blocks.first().expect("one block").label;
        module.functions.push(beam_loader::Function {
            func_label: entry,
            entry_label: entry,
            module: module.name,
            name: run_name,
            arity: 0,
            code,
            blocks,
            labels,
        });
        module.exports.push(beam_loader::Export {
            function: run_name,
            arity: 0,
            label: entry,
        });
        (module, run_name)
    };

    let resolver = {
        let callee = std::sync::Arc::clone(&callee);
        Box::new(move |module: &str, function: &str, arity: u32| {
            if module == "c" && function == "g" && arity == 1 {
                Ok(Some((std::sync::Arc::clone(&callee), callee_entry)))
            } else {
                Ok(None)
            }
        })
    };

    let mut machine = Machine::new();
    let messages = SharedMessages::default();
    let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
        messages: messages.clone(),
    }));
    ctx.set_external_resolver(resolver);
    let (func, pc) = match machine
        .run_export_outcome_with_tier(&caller, run, 0, &[], &mut ctx, Tier::Cranelift)
        .expect("run caller:run/0 on the Cranelift tier")
    {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("the callee's `receive` must park, got {other:?}"),
    };
    assert_eq!(
        machine.current_context(),
        1,
        "the park sits in the callee's own module context"
    );

    messages.push(make_small(5));
    let tiered = machine
        .resume_outcome_with_tier(&caller, func, pc, &mut ctx, Tier::Cranelift)
        .expect("the cranelift tier resumes inside the callee's context");

    // The interpreter's own park / resume of the same fixture is the reference.
    let mut reference = Machine::new();
    let reference_messages = SharedMessages::default();
    let mut reference_ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
        messages: reference_messages.clone(),
    }));
    reference_ctx.set_external_resolver({
        let callee = std::sync::Arc::clone(&callee);
        Box::new(move |module: &str, function: &str, arity: u32| {
            if module == "c" && function == "g" && arity == 1 {
                Ok(Some((std::sync::Arc::clone(&callee), callee_entry)))
            } else {
                Ok(None)
            }
        })
    });
    let (rfunc, rpc) = match reference
        .run_export_outcome_with_tier(
            &caller,
            run,
            0,
            &[],
            &mut reference_ctx,
            Tier::Interpreter,
        )
        .expect("interpret caller:run/0")
    {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("the callee's `receive` must park, got {other:?}"),
    };
    reference_messages.push(make_small(5));
    let expected = reference
        .resume_outcome(&caller, rfunc, rpc, &mut reference_ctx)
        .expect("the interpreter resume completes");
    assert_eq!((rfunc, rpc), (func, pc), "the same park address");
    assert_eq!(
        tiered, expected,
        "a resumed cranelift slice inside a callee must agree with the interpreter"
    );
    assert_eq!(tiered, RunOutcome::Returned(make_small(5)));
}

#[test]
fn a_resume_does_not_double_count_the_reduction_charge() {
    // Case 5 of the resume contract: the resumed slice's `Machine::reductions()` counts
    // **its own** charges only. Both tiers zero the counter at a slice boundary (the
    // interpreter's `Machine::drive`, the native driver's own reset in
    // `run_native_resume_outcome` behind `Machine::reset_reductions`), so the work the
    // parked slice had already done is not charged again — and the resumed slice's total
    // matches the interpreter's for the same park on a per-slice basis.
    cache::clear();
    let (module, run) = park_fixture();

    let mut machine = Machine::new();
    let mut ctx = ChargeCtx::with_budget(1);
    machine
        .run_export_outcome_with_tier(&module, run, 0, &[], &mut ctx, Tier::Cranelift)
        .expect_err("the second charge is refused");
    let parked_charges = ctx.charges.len();
    let (func, pc) = machine.current_code().expect("a published position");
    assert!(
        machine.reductions() >= 1,
        "the parked slice had charged at least once"
    );

    ctx.lift();
    let resumed = machine
        .resume_outcome_with_tier(&module, func, pc, &mut ctx, Tier::Cranelift)
        .expect("the resume completes");
    assert_eq!(resumed, RunOutcome::Returned(make_small(8)));
    let slice_charges = (ctx.charges.len() - parked_charges) as u64;
    assert!(slice_charges > 0, "the resumed slice did real work");
    assert_eq!(
        machine.reductions(),
        slice_charges,
        "the slice counter restarts at the resume instead of carrying the parked slice's \
         charges (which would count them twice)"
    );

    // The interpreter's resumed slice behaves identically: its counter is its own charges.
    let mut reference = Machine::new();
    let mut reference_ctx = ChargeCtx::with_budget(1);
    reference
        .run_export_outcome_with_tier(&module, run, 0, &[], &mut reference_ctx, Tier::Interpreter)
        .expect_err("the second charge is refused");
    let (ifunc, ipc) = reference.current_code().expect("a published position");
    let reference_parked = reference_ctx.charges.len();
    reference_ctx.lift();
    assert_eq!(
        reference
            .resume_outcome_with_tier(&module, ifunc, ipc, &mut reference_ctx, Tier::Interpreter)
            .expect("the interpreter resume completes"),
        resumed
    );
    assert_eq!(
        reference.reductions(),
        (reference_ctx.charges.len() - reference_parked) as u64,
        "the interpreter's slice counter is per-slice too — the two tiers agree on the rule"
    );
}
