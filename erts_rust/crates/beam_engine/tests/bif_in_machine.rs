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

//! The **in-machine BIF-dispatch seam** on **both execution tiers**.
//!
//! [`ExecutionContext::wants_bif_in_machine`] + [`ExecutionContext::call_bif_in_machine`]
//! are the door through which a host reads the *currently executing*
//! [`Machine`] from inside a BIF dispatch — the shape
//! `erlang:process_info(self(), current_stacktrace)` needs. The door is a `bool`
//! latch (`Machine::bif_in_machine_on`, set once per run by the driver that owns
//! the run), and the engine has **two** drivers: the interpreter's
//! (`Machine::drive`) and the native baseline tier's (`beam_engine::jit::baseline`).
//! A latch honoured by only one of them would make the same module dispatch its
//! BIFs through two different doors depending on the tier — silently, since a host
//! that overrides `call_bif_in_machine` would simply never be reached natively.
//!
//! This suite is the **tier-parity gate** for that seam: one hand-built module
//! dispatches a single zero-arity BIF on both tiers with a probe context that
//! records what it can read from the live machine, and the two captures must agree
//! exactly. The `bif0` is deliberately *not* in the baseline tier's inline set, so
//! the native run reaches the seam through the emitted step helper with the
//! runtime's entry module — the same funnel the interpreter uses.
//!
//! It also pins the **default** `false` probe: a host that only implements
//! [`ExecutionContext::call_bif`] keeps the plain door on both tiers, so the latch
//! cannot have changed any existing implementor's behaviour, and a failing BIF
//! still raises through the in-machine door exactly as it does through the plain one.

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext, RunOutcome, Tier};
use beam_loader::{Export, Function, Import, ImportRef, Instr, Label, Module, Operand};
use beam_term::{eq, make_atom, make_small, Atom, Eterm, NIL};

// ---------------------------------------------------------------------------
// The hand-built module
// ---------------------------------------------------------------------------

/// The synthetic module's name (the `M` of the entry MFA).
const MODULE_NAME: &str = "probe";
/// The entry export every test runs.
const ENTRY: &str = "run";
/// The imported, zero-arity BIF the entry dispatches.
const BIF_MODULE: &str = "host";
const BIF_FUNCTION: &str = "snapshot";
/// The value the probe BIF returns (and the module therefore returns from `run/0`).
const BIF_RESULT: i64 = 42;

/// The one-function fixture: `run/0` = `label 1 | bif0 host:snapshot/0 x0 | return`.
///
/// `bif0` has no fail label, so any BIF failure would raise; the probe BIF always
/// succeeds, so the run reaches the `return` with `x(0)` holding its result. The
/// basic blocks are built through the loader's own lowering so the native tier has
/// a real block to enter (and therefore a real deopt path to the seam).
fn probe_module() -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern(MODULE_NAME);
    module.imports.push(Import {
        module: module.atom_table.intern(BIF_MODULE),
        function: module.atom_table.intern(BIF_FUNCTION),
        arity: 0,
    });
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Bif0 {
            bif: ImportRef(0),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let (blocks, labels) = beam_loader::lower::build_blocks(&code);
    let name = module.atom_table.intern(ENTRY);
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(1),
        module: module.name,
        name,
        arity: 0,
        code,
        blocks,
        labels,
    });
    module.exports.push(Export {
        function: name,
        arity: 0,
        label: Label(1),
    });
    module
}

// ---------------------------------------------------------------------------
// The probe context
// ---------------------------------------------------------------------------

/// What the seam could read from the live machine during one BIF dispatch.
struct Capture {
    /// `Machine::current_stacktrace(entry_module, ctx)` — the cooked stacktrace.
    stacktrace: Eterm,
    /// `Machine::current_function(entry_module, ctx)`.
    current_function: Option<Eterm>,
}

/// A host that records which BIF door each dispatch took, and what the in-machine
/// door saw.
///
/// * `in_machine` is the answer the probe returns. `true` opts into the in-machine
///   door; `false` is the trait default every existing implementor has.
/// * `plain_calls` records every [`ExecutionContext::call_bif`] dispatch.
/// * `in_machine_calls` records every
///   [`ExecutionContext::call_bif_in_machine`] dispatch together with the capture.
/// * `arena` is the pointer-stable test heap the stacktrace builder allocates in.
struct ProbeCtx {
    in_machine: bool,
    plain_calls: Vec<(String, String, u32)>,
    in_machine_calls: Vec<(String, String, u32, Capture)>,
    arena: MinimalContext,
}

impl ProbeCtx {
    fn new(in_machine: bool) -> ProbeCtx {
        ProbeCtx {
            in_machine,
            plain_calls: Vec::new(),
            in_machine_calls: Vec::new(),
            arena: MinimalContext::new(),
        }
    }
}

impl ExecutionContext for ProbeCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        self.plain_calls
            .push((module.to_string(), function.to_string(), arity));
        Ok(make_small(BIF_RESULT))
    }

    fn wants_bif_in_machine(&self) -> bool {
        self.in_machine
    }

    fn call_bif_in_machine(
        &mut self,
        machine: &Machine,
        entry_module: &Module,
        module: &str,
        function: &str,
        arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        // The point of the seam: the host reads the running process's position from
        // *inside* the dispatch. Both accessors resolve context-`0` frames against
        // `entry_module`, which is exactly the module the run was entered at.
        let stacktrace = machine.current_stacktrace(entry_module, self)?;
        let current_function = machine.current_function(entry_module, self)?;
        self.in_machine_calls.push((
            module.to_string(),
            function.to_string(),
            arity,
            Capture {
                stacktrace,
                current_function,
            },
        ));
        Ok(make_small(BIF_RESULT))
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

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run `run/0` on `module` through `tier` on a fresh machine and a fresh probe
/// context, returning the outcome and the context so the caller can inspect which
/// door was taken.
fn run_on_tier(module: &Module, tier: Tier, in_machine: bool) -> (RunOutcome, ProbeCtx) {
    let mut ctx = ProbeCtx::new(in_machine);
    let mut machine = Machine::new();
    let run = atom(module, ENTRY);
    let outcome = machine
        .run_export_outcome_with_tier(module, run, 0, &[], &mut ctx, tier)
        .unwrap_or_else(|err| panic!("{tier:?}: the probe BIF must succeed, got {err:?}"));
    (outcome, ctx)
}

/// A name interned in `module`.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

/// Assert `element` is the atom `name` (resolved through `module`'s atom table).
fn assert_atom_element(module: &Module, element: Eterm, name: &str, what: &str) {
    assert!(
        eq(element, make_atom(atom(module, name)), &module.atom_table),
        "{what}: expected the atom {name}, got {element:?}"
    );
}

/// Assert the capture names `{probe, run, 0}` — the entry MFA — in both accessors,
/// with the synthetic module's empty location (`[]`, no `Line` chunk).
fn assert_capture_names_the_entry(module: &Module, capture: &Capture) {
    // The cooked stacktrace: exactly the one live frame — the entry function.
    let frames =
        terms::list_elements(capture.stacktrace).expect("a current_stacktrace is a proper list");
    assert_eq!(
        frames.len(),
        1,
        "a fresh run has exactly the executing frame, got {frames:?}"
    );
    assert_entry_item(module, frames[0], "current_stacktrace item");

    // `current_function` is the `{M,F,A}` of the same frame.
    let current = capture
        .current_function
        .expect("a machine inside a BIF dispatch has a current function");
    assert_eq!(
        terms::tuple_arity(current).expect("an MFA is a tuple"),
        3,
        "current_function is a 3-tuple"
    );
    assert_atom_element(
        module,
        terms::tuple_element(current, 0).expect("the M element"),
        MODULE_NAME,
        "current_function module",
    );
    assert_atom_element(
        module,
        terms::tuple_element(current, 1).expect("the F element"),
        ENTRY,
        "current_function function",
    );
    assert_eq!(
        terms::tuple_element(current, 2).expect("the A element"),
        make_small(0),
        "current_function arity"
    );
}

/// Assert `item` is the `{probe, run, 0, []}` stacktrace item.
fn assert_entry_item(module: &Module, item: Eterm, what: &str) {
    assert_eq!(
        terms::tuple_arity(item).expect("a stacktrace item is a tuple"),
        4,
        "{what} is a {{M,F,A,Location}} 4-tuple"
    );
    assert_atom_element(
        module,
        terms::tuple_element(item, 0).expect("the M element"),
        MODULE_NAME,
        what,
    );
    assert_atom_element(
        module,
        terms::tuple_element(item, 1).expect("the F element"),
        ENTRY,
        what,
    );
    assert_eq!(
        terms::tuple_element(item, 2).expect("the A element"),
        make_small(0),
        "{what} arity"
    );
    let location = terms::tuple_element(item, 3).expect("the Location element");
    assert!(
        eq(location, NIL, &module.atom_table),
        "{what}: a synthetic module has no Line chunk, so Location is [], got {location:?}"
    );
}

/// The single `(module, function, arity, capture)` the run dispatched its BIF with.
fn only_call(calls: &[(String, String, u32, Capture)]) -> &(String, String, u32, Capture) {
    assert_eq!(
        calls.len(),
        1,
        "the entry dispatches exactly one BIF, got {}",
        calls.len()
    );
    &calls[0]
}

// ---------------------------------------------------------------------------
// (a) the seam sees the live machine on BOTH tiers
// ---------------------------------------------------------------------------

#[test]
fn the_in_machine_door_reaches_the_host_on_both_tiers() {
    let module = probe_module();
    let (interpreted, interp_ctx) = run_on_tier(&module, Tier::Interpreter, true);
    let (jitted, jit_ctx) = run_on_tier(&module, Tier::BaselineJit, true);

    // The module returns the BIF's own result on both tiers.
    assert_eq!(interpreted, RunOutcome::Returned(make_small(BIF_RESULT)));
    assert_eq!(jitted, interpreted, "the two tiers must agree");

    // Both tiers routed the dispatch through `call_bif_in_machine` — never the
    // plain door. This is the assertion the whole suite exists for: a latch
    // honoured by only one driver shows up here as a non-empty `plain_calls`.
    assert!(
        interp_ctx.plain_calls.is_empty(),
        "the interpreter took the plain door: {:?}",
        interp_ctx.plain_calls
    );
    assert!(
        jit_ctx.plain_calls.is_empty(),
        "the baseline tier took the plain door: {:?}",
        jit_ctx.plain_calls
    );

    let interp_call = only_call(&interp_ctx.in_machine_calls);
    let jit_call = only_call(&jit_ctx.in_machine_calls);
    assert_eq!(
        (interp_call.0.as_str(), interp_call.1.as_str(), interp_call.2),
        (BIF_MODULE, BIF_FUNCTION, 0),
        "the in-machine door must see the same BIF dispatch as any host"
    );
    assert_eq!(
        (jit_call.0.as_str(), jit_call.1.as_str(), jit_call.2),
        (BIF_MODULE, BIF_FUNCTION, 0),
        "the native tier must see the same BIF dispatch"
    );

    // What the host read from the live machine: both accessors name the entry
    // module's function, on both tiers.
    assert_capture_names_the_entry(&module, &interp_call.3);
    assert_capture_names_the_entry(&module, &jit_call.3);

    // And the two captures are identical — structurally, atom for atom.
    assert!(
        eq(
            interp_call.3.stacktrace,
            jit_call.3.stacktrace,
            &module.atom_table
        ),
        "the interpreter's stacktrace {:?} differs from the baseline tier's {:?}",
        interp_call.3.stacktrace,
        jit_call.3.stacktrace
    );
    assert!(
        eq(
            interp_call
                .3
                .current_function
                .expect("an MFA on the interpreter"),
            jit_call.3.current_function.expect("an MFA natively"),
            &module.atom_table
        ),
        "the two tiers' current_function must be the same MFA"
    );
}

// ---------------------------------------------------------------------------
// (b) the default probe keeps the plain `call_bif` path, on both tiers
// ---------------------------------------------------------------------------

#[test]
fn the_default_probe_keeps_the_plain_call_bif_path_on_both_tiers() {
    let module = probe_module();
    let (interpreted, interp_ctx) = run_on_tier(&module, Tier::Interpreter, false);
    let (jitted, jit_ctx) = run_on_tier(&module, Tier::BaselineJit, false);

    assert_eq!(interpreted, RunOutcome::Returned(make_small(BIF_RESULT)));
    assert_eq!(jitted, interpreted, "the two tiers must agree");

    let expected = vec![(BIF_MODULE.to_string(), BIF_FUNCTION.to_string(), 0)];
    assert_eq!(
        interp_ctx.plain_calls, expected,
        "the default probe must leave the interpreter on the plain door"
    );
    assert_eq!(
        jit_ctx.plain_calls, expected,
        "the default probe must leave the native tier on the plain door"
    );
    assert!(
        interp_ctx.in_machine_calls.is_empty(),
        "the in-machine door must not be taken without the opt-in"
    );
    assert!(
        jit_ctx.in_machine_calls.is_empty(),
        "the in-machine door must not be taken without the opt-in"
    );
}

// ---------------------------------------------------------------------------
// (c) the door is a dispatch door, not an error-handling one
// ---------------------------------------------------------------------------

/// A probe that opts into the in-machine door but fails the BIF: `bif0` carries no
/// fail label, so the error must propagate exactly as the plain door's does.
struct FailingCtx {
    arena: MinimalContext,
}

impl ExecutionContext for FailingCtx {
    fn call_bif(
        &mut self,
        _module: &str,
        _function: &str,
        _arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        Err(BeamError::BadArg)
    }

    fn wants_bif_in_machine(&self) -> bool {
        true
    }

    fn call_bif_in_machine(
        &mut self,
        machine: &Machine,
        entry_module: &Module,
        _module: &str,
        _function: &str,
        _arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        // The seam is still handed the live machine on the failing path.
        machine.current_stacktrace(entry_module, self)?;
        Err(BeamError::BadArg)
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

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }
}

#[test]
fn a_failing_bif_still_raises_through_the_in_machine_door_on_both_tiers() {
    let module = probe_module();
    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let mut ctx = FailingCtx {
            arena: MinimalContext::new(),
        };
        let mut machine = Machine::new();
        let run = atom(&module, ENTRY);
        let err = machine
            .run_export_outcome_with_tier(&module, run, 0, &[], &mut ctx, tier)
            .expect_err("a failing BIF must not be folded into a value");
        assert!(
            matches!(err, BeamError::BadArg),
            "{tier:?}: the failing BIF's typed error must propagate unchanged, got {err:?}"
        );
    }
}
