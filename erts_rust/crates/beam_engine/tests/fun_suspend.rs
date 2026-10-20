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
//! **The engine-level closure-apply suspend channel**: park a process on a
//! `call_fun`/`call_fun2` whose application cannot complete without waiting, and
//! resume it by **re-consulting the seam with the same fun and arguments**.
//!
//! [`ExecutionContext::apply_fun_outcome`] may answer [`FunOutcome::Suspend`] instead
//! of a value. The engine then applies **nothing**: it reports
//! `RunOutcome::Suspend { func, pc }` at the *`call_fun` instruction itself*, and
//! [`Machine::resume_outcome`] re-enters there, so the host is asked a second time —
//! for the *pending* application, which it must continue rather than restart. That is
//! the whole contract this suite pins, one property per test:
//!
//! | test | what it proves |
//! |---|---|
//! | [`a_suspending_application_parks_on_the_call_fun_instruction`] | the reported address is the `call_fun` instruction (`Machine::current_code` too) and `x(0)` is untouched |
//! | [`a_resumed_application_is_continued_with_the_same_fun_and_arguments`] | the second consultation sees the identical `fun` term and the identical `args`, and nothing between the two was re-applied |
//! | [`both_tiers_report_and_resume_the_same_suspend`] | interpreter and baseline JIT report and resume the same park, and agree on the value |
//! | [`the_non_blocking_entries_report_a_suspend_as_a_typed_error`] | `run_export`/`run_label` fold the park into `error::suspended()` (not `blocked()`) |
//! | [`the_defaulted_door_keeps_a_plain_host_byte_identical`] | a host that overrides only `apply_fun` returns that value; a host that overrides neither yields the same `unsupported("apply_fun")` as before |
//! | [`the_in_machine_closure_seam_wins_and_the_outcome_door_is_never_consulted`] | a host that resolves the closure's lambda never reaches the outcome door |
//!
//! It is the closure-apply twin of `tests/bif_suspend.rs`, which pins the same
//! properties for [`BifOutcome`]/`call_bif_outcome`; the two channels differ in ONE
//! respect, and it is the reason the host contract is stricter here: a suspended BIF
//! is **re-issued** (re-runnable from its operands), while a suspended *application*
//! is **continued** (an evaluator body is not re-execution idempotent).
//!
//! # Rooting
//!
//! The engine keeps **no** suspended-application state: neither the closure nor the
//! arguments are copied. The parked state is `(func, pc)` plus the machine's registers,
//! Erlang stack and FR file — exactly the frozen
//! [`RootSet`](beam_engine::context::RootSet) a collecting host is handed at every
//! admission point — and the re-execution re-reads the fun from `x(arity)` and the
//! arguments from `x(0..arity)`. So the suspended operands are roots by construction.
use std::sync::Arc;
use beam_common::{BeamError, Result};
use beam_engine::{
    error, terms, ExecutionContext, FunOutcome, Machine, MinimalContext, RunOutcome, Tier,
};
use beam_loader::{
    Export, FunDescriptor, Function, HeapNeed, Instr, Label, Module, Operand,
};
use beam_term::{make_small, Atom as TermAtom, Eterm};
// ---------------------------------------------------------------------------
// The hand-built fixture
// ---------------------------------------------------------------------------
/// The fixture module's name.
const MODULE_NAME: &str = "funmod";
/// The fixture's only export (`run/0`).
const ENTRY: &str = "run";
/// The call argument the fixture puts in `x(0)` before the `call_fun`.
const ARGUMENT: i64 = 10;
/// The value the host answers once it stops suspending — deliberately distinct from
/// [`ARGUMENT`], so "the instruction was not applied" is observable in `x(0)`.
const RESULT: i64 = 999;
/// The fixture has one function, so every `func` is `0`.
const FUNC: u32 = 0;
/// `funmod:run/0` — the oracle's `f(F) -> F(10)` with the closure built in place:
///
/// ```text
///  0: label 1
///  1: label 2
///  2: allocate 0 2                   ; a frame (the closure seam stores a continuation on it)
///  3: move 7 x(0)                    ; the captured free variable
///  4: make_fun3 <funT> x(1) [x(0)]   ; x(1) = fun(A) -> … end   (num_free = 1)
///  5: move 10 x(0)                   ; the call argument
///  6: call_fun 1                     ; <- the parking instruction
///  7: deallocate 0
///  8: return
/// ```
///
/// The closure is built by real code (`make_fun3`), not handed in as an argument, so a
/// resume that re-entered at the wrong `pc` would rebuild it — and the re-consultation
/// would see a **different** fun word. That is what makes the "continued, not
/// restarted" assertion sharp.
///
/// Every basic block is built through the loader's own lowering, so the native tier has
/// real blocks to enter.
fn fun_module() -> (Module, TermAtom) {
    let mut module = Module::default();
    module.name = module.atom_table.intern(MODULE_NAME);
    let lambda = module.atom_table.intern("lambda");
    module.funs.push(FunDescriptor {
        function: lambda,
        arity: 2,
        label: Label(1),
        index: 0,
        num_free: 1,
        uniq: 0xCAFE,
    });
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(0),
            live: 2,
        },
        Instr::Move {
            src: Operand::Integer(7),
            dst: Operand::X(0),
        },
        Instr::MakeFun3 {
            index: 0,
            dst: Operand::X(1),
            env: vec![Operand::X(0)],
        },
        Instr::Move {
            src: Operand::Integer(ARGUMENT),
            dst: Operand::X(0),
        },
        Instr::CallFun { arity: 1 },
        Instr::Deallocate(0),
        Instr::Return,
    ];
    let (blocks, labels) = beam_loader::lower::build_blocks(&code);
    let name = module.atom_table.intern(ENTRY);
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(2),
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
        label: Label(2),
    });
    (module, name)
}
/// The index of the fixture's `call_fun` instruction — the address a park must report.
///
/// Derived from the module's own code (never a literal), so the assertion states "the
/// park is on the `call_fun`", not "the park is at pc 6".
fn call_fun_pc(module: &Module) -> u32 {
    let pc = module.functions[0]
        .code
        .iter()
        .position(|instr| matches!(instr, Instr::CallFun { .. }))
        .expect("the fixture has a `call_fun`");
    pc as u32
}
/// A second module whose only function is a **lambda body** the host can answer the
/// in-machine closure seam with: `lambda/2` ignores its arguments and returns
/// [`LAMBDA_RESULT`].
const LAMBDA_MODULE: &str = "lambdamod";
/// The value the in-machine lambda body returns.
const LAMBDA_RESULT: i64 = 77;
fn lambda_module() -> Arc<Module> {
    let mut module = Module::default();
    module.name = module.atom_table.intern(LAMBDA_MODULE);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Move {
            src: Operand::Integer(LAMBDA_RESULT),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let (blocks, labels) = beam_loader::lower::build_blocks(&code);
    let name = module.atom_table.intern("lambda");
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(1),
        module: module.name,
        name,
        arity: 2,
        code,
        blocks,
        labels,
    });
    Arc::new(module)
}
// ---------------------------------------------------------------------------
// The hosts
// ---------------------------------------------------------------------------
/// One consultation of the closure-apply door, as the host saw it.
type Application = (Eterm, Vec<Eterm>);
/// A test host whose closure application can **wait**: it answers
/// [`FunOutcome::Suspend`] for its first `suspends_left` consultations and the value
/// [`RESULT`] afterwards.
///
/// It records every consultation (so "the same fun and the same arguments" is
/// checkable), every `resolve_local_fun` consultation, and which of the two
/// apply doors each consultation took.
struct ParkingHost {
    /// The pointer-stable test heap the engine's term builders allocate in.
    arena: MinimalContext,
    /// How many more consultations answer [`FunOutcome::Suspend`].
    suspends_left: usize,
    /// Every consultation of [`ExecutionContext::apply_fun_outcome`], in order.
    applications: Vec<Application>,
    /// Every consultation of the value-returning [`ExecutionContext::apply_fun`].
    plain_applications: Vec<Application>,
    /// How many times [`ExecutionContext::resolve_local_fun`] was consulted.
    resolve_calls: usize,
    /// The callee the in-machine closure seam answers, or `None` to decline.
    callee: Option<Arc<Module>>,
}
impl ParkingHost {
    /// A host that suspends `suspends_left` times before answering the value.
    fn new(suspends_left: usize) -> ParkingHost {
        ParkingHost {
            arena: MinimalContext::new(),
            suspends_left,
            applications: Vec::new(),
            plain_applications: Vec::new(),
            resolve_calls: 0,
            callee: None,
        }
    }
    /// The same host, answering the in-machine closure seam with [`lambda_module`].
    fn resolving(suspends_left: usize) -> ParkingHost {
        ParkingHost {
            callee: Some(lambda_module()),
            ..ParkingHost::new(suspends_left)
        }
    }
    /// Record a consultation, then suspend while the budget lasts.
    fn answer(&mut self, fun: Eterm, args: &[Eterm]) -> FunOutcome {
        self.applications.push((fun, args.to_vec()));
        if self.suspends_left > 0 {
            self.suspends_left -= 1;
            return FunOutcome::Suspend;
        }
        FunOutcome::Return(make_small(RESULT))
    }
}
impl ExecutionContext for ParkingHost {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        panic!("the fixture dispatch of {module}:{function}/{arity} must never reach a BIF")
    }
    /// The value-returning door must **never** be reached by this host: it overrides
    /// the outcome-aware one, which is the door the engine consults.
    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        self.plain_applications.push((fun, args.to_vec()));
        panic!("a host that overrides `apply_fun_outcome` must not see the plain door")
    }
    fn apply_fun_outcome(&mut self, fun: Eterm, args: &[Eterm]) -> Result<FunOutcome> {
        Ok(self.answer(fun, args))
    }
    fn resolve_local_fun(
        &mut self,
        _fun: Eterm,
        _args: &[Eterm],
    ) -> Result<Option<(Arc<Module>, Label)>> {
        self.resolve_calls += 1;
        Ok(self.callee.clone().map(|callee| (callee, Label(1))))
    }
    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }
    fn alloc_words_rooted(&mut self, n: usize, _roots: beam_engine::context::RootSet<'_>) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }
}
/// A host that overrides only the **value-returning** door: the defaulted
/// `apply_fun_outcome` must forward to it and wrap the value, leaving the run
/// byte-identical to a run that predates the channel.
struct DefaultDoorHost {
    /// The pointer-stable test heap.
    arena: MinimalContext,
    /// Every consultation of the value-returning door.
    plain_applications: Vec<Application>,
}
impl DefaultDoorHost {
    fn new() -> DefaultDoorHost {
        DefaultDoorHost {
            arena: MinimalContext::new(),
            plain_applications: Vec::new(),
        }
    }
}
impl ExecutionContext for DefaultDoorHost {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        panic!("the fixture dispatch of {module}:{function}/{arity} must never reach a BIF")
    }
    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        self.plain_applications.push((fun, args.to_vec()));
        Ok(make_small(RESULT))
    }
    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }
}
// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
/// The typed reason atom of an engine raise.
fn reason(error: BeamError) -> String {
    match error {
        BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .unwrap_or_else(|| panic!("the engine's typed error names its reason"))
            .to_string(),
        other => panic!("expected a typed raise, got {other:?}"),
    }
}
/// Run the fixture's `run/0` to its **first** park (or result).
fn first_slice(
    module: &Module,
    name: TermAtom,
    ctx: &mut ParkingHost,
    tier: Tier,
) -> Result<(RunOutcome, Machine)> {
    let mut machine = Machine::new();
    let outcome = machine.run_export_outcome_with_tier(module, name, 0, &[], ctx, tier)?;
    Ok((outcome, machine))
}
/// Drive the fixture's `run/0` from entry to `RunOutcome::Returned`, resuming at every
/// reported park — the host's whole pump loop.
fn drive_to_value(
    module: &Module,
    name: TermAtom,
    ctx: &mut ParkingHost,
    tier: Tier,
) -> Result<(Eterm, Machine, usize)> {
    let mut machine = Machine::new();
    let mut outcome = machine.run_export_outcome_with_tier(module, name, 0, &[], ctx, tier)?;
    let mut parks = 0usize;
    while let RunOutcome::Suspend { func, pc } = outcome {
        parks += 1;
        assert!(
            parks <= 8,
            "the fixture parked more than 8 times; the host's pump must make progress"
        );
        outcome = machine.resume_outcome_with_tier(module, func, pc, ctx, tier)?;
    }
    match outcome {
        RunOutcome::Returned(value) => Ok((value, machine, parks)),
        other => panic!("the resumed run must finish with a value, got {other:?}"),
    }
}
// ---------------------------------------------------------------------------
// (1) the park address
// ---------------------------------------------------------------------------
/// A suspend parks on the **`call_fun` instruction itself**, with nothing applied.
///
/// `x(0)` is the crispest proof: it holds the call argument before the application, and
/// a suspend must leave it there — a value would have overwritten it with
/// [`RESULT`], and a `pc + 1` resume would have skipped the instruction.
#[test]
fn a_suspending_application_parks_on_the_call_fun_instruction() {
    let (module, name) = fun_module();
    let expected_pc = call_fun_pc(&module);
    assert!(
        matches!(
            module.functions[0].code[expected_pc as usize],
            Instr::CallFun { arity: 1 }
        ),
        "the parked instruction must be the fixture's `call_fun 1`"
    );
    let mut ctx = ParkingHost::new(1);
    let (outcome, machine) = first_slice(&module, name, &mut ctx, Tier::Interpreter)
        .expect("the fixture must park, not fail");
    assert_eq!(
        outcome,
        RunOutcome::Suspend {
            func: FUNC,
            pc: expected_pc
        },
        "an application suspend must report the `call_fun` instruction's own address, not the next \
         one: resumption re-executes that instruction"
    );
    assert_eq!(
        machine.current_code(),
        Some((FUNC, expected_pc)),
        "the machine's recorded execution position must be the parking instruction, so a host can \
         read it back through the introspection accessors"
    );
    assert_eq!(
        machine.x(0),
        Some(make_small(ARGUMENT)),
        "a suspended application is NOT applied: x(0) must still hold the call argument, never the \
         value the abandoned attempt would have computed"
    );
    assert_eq!(
        ctx.applications.len(),
        1,
        "the host must see exactly one consultation before the park"
    );
    assert_eq!(
        ctx.plain_applications.len(),
        0,
        "the engine must consult the outcome-aware door, never the value-returning one"
    );
    assert_eq!(
        ctx.resolve_calls, 1,
        "the in-machine closure seam is consulted first, exactly once per execution of the \
         `call_fun`"
    );
}
// ---------------------------------------------------------------------------
// (2) the continued application
// ---------------------------------------------------------------------------
/// Resumption **continues** the same application: the second consultation receives the
/// identical `fun` term and the identical arguments, and no instruction between the two
/// consultations was re-applied.
///
/// The fun is built by the fixture's own `make_fun3` (`pc 4`), so an unchanged fun word
/// is also the proof that the resume re-entered at the `call_fun` and not earlier: a
/// re-executed `make_fun3` would allocate a **new** closure.
#[test]
fn a_resumed_application_is_continued_with_the_same_fun_and_arguments() {
    let (module, name) = fun_module();
    let expected_pc = call_fun_pc(&module);
    let mut ctx = ParkingHost::new(1);
    let (outcome, mut machine) =
        first_slice(&module, name, &mut ctx, Tier::Interpreter).expect("the fixture must park");
    let RunOutcome::Suspend { func, pc } = outcome else {
        panic!("expected an application park, got {outcome:?}");
    };
    assert_eq!((func, pc), (FUNC, expected_pc));
    let resumed = machine
        .resume_outcome(&module, func, pc, &mut ctx)
        .expect("the continued application returns a value, so the run must finish");
    assert_eq!(
        ctx.applications.len(),
        2,
        "a park plus a resume is exactly two consultations of the pending application: one that \
         suspended and one that continues it — a third would mean the engine retried"
    );
    assert_eq!(
        ctx.applications[0].0, ctx.applications[1].0,
        "the continuation must be handed the SAME fun term: the engine holds no suspended- \
         application state, so it re-reads `x(arity)`, and a new word would mean the closure was \
         rebuilt (a re-executed `make_fun3`), i.e. restarted instead of continued"
    );
    assert_eq!(
        ctx.applications[0].1, ctx.applications[1].1,
        "the continuation must be handed the SAME arguments: they are re-read from `x(0..arity)` \
         and nothing wrote them in between"
    );
    assert_eq!(
        ctx.applications[0].1,
        vec![make_small(ARGUMENT)],
        "the arguments are the fixture's own `x(0)` value"
    );
    // Nothing between the two consultations was applied: the only register the
    // application may write (`x(0)`, success-only) still holds the argument at the park,
    // and the run then finishes with the continued application's value.
    assert_eq!(
        machine.x(0),
        Some(make_small(RESULT)),
        "after the continuation the value lands in x(0), exactly as a synchronously applied \
         closure's would"
    );
    assert_eq!(
        resumed,
        RunOutcome::Returned(make_small(RESULT)),
        "the resumed run must finish with the continued application's value"
    );
    assert!(
        machine.pending_exception().is_none(),
        "a park is not an exception: the machine must end with no pending exception"
    );
}
// ---------------------------------------------------------------------------
// (3) tier parity
// ---------------------------------------------------------------------------
/// Both execution tiers report the **same** park and resume it identically.
///
/// A tier that could park but not resume would strand a host that picked it, and the
/// block ABI's `SUSPEND` sentinel must carry the suspend of a **family-owned**
/// instruction (`call_fun`), not only that of a per-instruction BIF deopt.
#[test]
fn both_tiers_report_and_resume_the_same_suspend() {
    let (module, name) = fun_module();
    let expected_pc = call_fun_pc(&module);
    let mut outcomes = Vec::new();
    let mut values = Vec::new();
    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let mut ctx = ParkingHost::new(1);
        let (outcome, mut machine) =
            first_slice(&module, name, &mut ctx, tier).expect("the fixture must park on every tier");
        let RunOutcome::Suspend { func, pc } = outcome else {
            panic!("{tier:?}: expected an application park, got {outcome:?}");
        };
        assert_eq!(
            (func, pc),
            (FUNC, expected_pc),
            "{tier:?}: the park must be the `call_fun` instruction"
        );
        assert_eq!(
            machine.x(0),
            Some(make_small(ARGUMENT)),
            "{tier:?}: a suspend applies nothing, so x(0) must still hold the argument"
        );
        outcomes.push((tier, outcome));
        let resumed = machine
            .resume_outcome_with_tier(&module, func, pc, &mut ctx, tier)
            .unwrap_or_else(|err| panic!("{tier:?}: the resumed run must finish, got {err:?}"));
        assert_eq!(
            ctx.applications.len(),
            2,
            "{tier:?}: a park and a resume are two consultations on every tier"
        );
        assert_eq!(
            ctx.applications[0], ctx.applications[1],
            "{tier:?}: the continued application must see the same fun and arguments on every tier"
        );
        match resumed {
            RunOutcome::Returned(value) => values.push((tier, value)),
            other => panic!("{tier:?}: the resumed run must return a value, got {other:?}"),
        }
    }
    assert_eq!(
        outcomes[0].1, outcomes[1].1,
        "the {:?} and {:?} tiers must report the SAME park address",
        outcomes[0].0, outcomes[1].0
    );
    assert_eq!(
        values[0].1, values[1].1,
        "the two tiers must resume to the same value ({} vs {}): the tier seam may change the code \
         path, never the result",
        values[0].0.name(),
        values[1].0.name()
    );
    assert_eq!(
        values[0].1,
        make_small(RESULT),
        "the shared value is the continued application's"
    );
}
// ---------------------------------------------------------------------------
// (4) the non-blocking entries
// ---------------------------------------------------------------------------
/// The non-blocking entries fold a park into `error::suspended()` — not `blocked()`.
///
/// A host reading the reason atom must be able to tell "the mailbox was empty" from
/// "this application needs pumping": the two have different release conditions.
#[test]
fn the_non_blocking_entries_report_a_suspend_as_a_typed_error() {
    let (module, name) = fun_module();
    let mut ctx = ParkingHost::new(1);
    let mut machine = Machine::new();
    let export_err = machine
        .run_export(&module, name, 0, &[], &mut ctx)
        .expect_err("`run_export` cannot suspend a process");
    let export_reason = reason(export_err);
    assert_eq!(
        export_reason,
        "unsupported_opcode: run: process suspended by a BIF (host must resume)",
        "`run_export` must fold the park into the typed `suspended()` error"
    );
    assert_ne!(
        export_reason,
        reason(error::blocked()),
        "an application suspend must stay distinguishable from an empty-mailbox `receive`: they \
         are released by different events"
    );
    let mut ctx = ParkingHost::new(1);
    let mut machine = Machine::new();
    let label_err = machine
        .run_label(&module, Label(2), 0, &[], &mut ctx)
        .expect_err("`run_label` cannot suspend a process");
    assert_eq!(
        reason(label_err),
        export_reason,
        "`run_label` must fold the park into the same typed error as `run_export`"
    );
    assert_eq!(
        ctx.applications.len(),
        1,
        "each non-blocking entry consults the host once and stops: it never retries a park"
    );
}
// ---------------------------------------------------------------------------
// (5) the defaulted door
// ---------------------------------------------------------------------------
/// A host that overrides only `apply_fun` behaves exactly as it did before the channel
/// existed, and a host that overrides neither still reports the same typed error.
#[test]
fn the_defaulted_door_keeps_a_plain_host_byte_identical() {
    let (module, name) = fun_module();
    // (a) `apply_fun` overridden, `apply_fun_outcome` defaulted: the value must arrive
    //     wrapped in `FunOutcome::Return` and the instruction must apply.
    let mut ctx = DefaultDoorHost::new();
    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome(&module, name, 0, &[], &mut ctx)
        .expect("a returning application must not disturb the run");
    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(RESULT)),
        "the defaulted outcome door must forward to `apply_fun` and wrap the value in \
         `FunOutcome::Return`, leaving the run byte-identical"
    );
    assert_eq!(
        machine.x(0),
        Some(make_small(RESULT)),
        "the destination register must be written: `x(0)` then `pc + 1` on the default path"
    );
    assert_eq!(
        ctx.plain_applications.len(),
        1,
        "the defaulted door consults `apply_fun` exactly once"
    );
    assert_eq!(
        ctx.plain_applications[0].1,
        vec![make_small(ARGUMENT)],
        "the forwarded call carries the fixture's own argument"
    );
    assert!(terms::is_fun(ctx.plain_applications[0].0), "and its fun term");
    // (b) neither overridden: `MinimalContext` has no apply hook, so the trait default
    //     `apply_fun` refuses — the SAME typed capability gap as before the channel.
    let mut ctx = MinimalContext::new();
    let mut machine = Machine::new();
    let err = machine
        .run_export_outcome(&module, name, 0, &[], &mut ctx)
        .expect_err("an unimplemented `apply_fun` is a typed gap");
    assert_eq!(
        reason(err),
        "unsupported_opcode: apply_fun",
        "a host that implements neither door must yield exactly the pre-existing \
         `unsupported(\"apply_fun\")` error: the new door only forwards"
    );
}
// ---------------------------------------------------------------------------
// (6) the in-machine closure seam wins
// ---------------------------------------------------------------------------
/// When the host resolves the closure's lambda, the engine enters it **in this
/// machine** — so the apply outcome door is never consulted.
///
/// The precedence matters: a host that can run the closure's real code must not have it
/// interpreted through its own fallback, whose park the engine would then have to pump
/// instead of running the body.
#[test]
fn the_in_machine_closure_seam_wins_and_the_outcome_door_is_never_consulted() {
    let (module, name) = fun_module();
    // `suspends_left = 1` is a trap: if the outcome door were reached, the run would
    // park (and the assertions below would fail loudly) instead of returning the lambda's
    // value.
    let mut ctx = ParkingHost::resolving(1);
    let (value, _machine, parks) = drive_to_value(&module, name, &mut ctx, Tier::Interpreter)
        .expect("the resolved lambda runs in-machine");
    assert_eq!(
        parks, 0,
        "a resolved closure is entered in this machine: the run must not park"
    );
    assert_eq!(
        value,
        make_small(LAMBDA_RESULT),
        "the in-machine lambda body's value must be the run's result"
    );
    assert_eq!(
        ctx.applications.len(),
        0,
        "`resolve_local_fun` wins: the outcome-aware apply door must never be consulted"
    );
    assert_eq!(
        ctx.plain_applications.len(),
        0,
        "and neither must the value-returning one"
    );
    assert_eq!(
        ctx.resolve_calls, 1,
        "the seam is consulted exactly once for the single `call_fun`"
    );
}
