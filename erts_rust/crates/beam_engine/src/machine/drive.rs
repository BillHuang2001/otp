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

//! The module-context-aware dispatch driver.
//!
//! [`Machine::drive`] is the loop that actually runs instructions: it owns the
//! `(func, pc)` position of the run and the **module context** those indices
//! belong to, and folds every [`Step`] the interpreter or an instruction family
//! returns back into that position.
//!
//! # Why the driver, not the loops, owns the context
//!
//! A `call_ext*` the host resolved through
//! [`ExecutionContext::resolve_external_call`](crate::ExecutionContext::resolve_external_call)
//! is executed **in this machine** (see [`crate::machine::context`]): the callee's
//! instructions live in another module, so the loop's `(module, labels)` pair is no
//! longer fixed for the run. The `call_ext*` arms report the transfer as
//! [`Step::Enter`], the frame model records a continuation's module context on the
//! frame itself ([`Continuation::Address`]), and this loop re-binds the active
//! module (and the machine's literal area) at those points only — an intra-module
//! [`Step::Goto`] still costs nothing.
//!
//! The loop keeps the entry module **borrowed** (`&Module`, context `0`) and every
//! entered module **owned** through its [`ModuleContext`] handle, so the module
//! identity of a suspended frame survives a
//! [`RunOutcome::Wait`](crate::RunOutcome::Wait) — which is exactly what
//! [`Machine::resume_outcome`] needs, because it is deliberately not a
//! [`reset`](Machine::reset).

use std::sync::Arc;

use beam_common::Result;
use beam_loader::Module;

use crate::context::ExecutionContext;
use crate::error;
use crate::interpreter::{build_label_map, LabelMap};
use crate::Step;

use super::{Continuation, Machine, ModuleContext, RunOutcome};

impl Machine {
    /// The dispatch loop over a pre-built label map, with the module context the
    /// machine currently carries.
    ///
    /// * `module` / `labels` describe the **entry** module — context `0`. The map may
    ///   be handed in (a run entry that already resolved its own entry label) or
    ///   built here, exactly once.
    /// * The starting context is [`Machine::current_context`]: `0` for a fresh run,
    ///   and the context a suspended process blocked in for a resume. A non-zero
    ///   context must still be registered in [`Machine::contexts`] (a resume does not
    ///   reset, so it is), and its module is used for `(func, pc)` — the passed
    ///   `module` is only context `0`'s module.
    ///
    /// Reports a `receive` that finds no message as [`RunOutcome::Wait`] (leaving the
    /// machine untouched at the reported address, and the context in
    /// [`Machine::current_context`] for the resume), and a `return`/halt as
    /// [`RunOutcome::Returned`].
    ///
    /// Also latches the A.8 tracing gate **once per run**: the per-instruction
    /// call/return arms read the plain `Machine::trace_on` `bool` instead of making a
    /// `trace_enabled()` vtable call per instruction, so an untraced run pays a
    /// single `bool` load and no event is ever built. See the `Machine::trace_on`
    /// field docs for the mid-run semantics.
    ///
    /// And latches the **in-machine BIF-dispatch gate**
    /// ([`Machine::bif_in_machine_on`], the cached
    /// [`ExecutionContext::wants_bif_in_machine`] answer) so every BIF dispatch in the
    /// run funnels through [`Machine::dispatch_bif_call`] and the host either sees the
    /// live machine on each one or keeps the plain `call_bif` path. The native
    /// baseline tier's own driver latches the same gate, so the door behaves
    /// identically on both tiers.
    ///
    /// And charges **one reduction per interpreted instruction, before the
    /// instruction's effect**: the slice's count starts at zero here and is read
    /// back through [`Machine::reductions`], while each charge is also offered to
    /// the host through [`ExecutionContext::reduce`] — the only way a
    /// **non-allocating** loop can be pre-empted, since `ensure_heap` is never
    /// reached by one. Because the charge precedes the body, an `Err` from the hook
    /// ends the run **without** applying the instruction: the machine stays parked
    /// at `(func, pc)` ([`Machine::current_code`]), so re-entering through
    /// [`Machine::resume_outcome`] applies it **exactly once**. The counter thus also
    /// counts a raising instruction and the charge the slice pre-empted on (the same
    /// charge the host saw — the two always agree). An `Err` from the hook is never
    /// swallowed: it ends the run exactly like any other engine failure.
    pub(crate) fn drive(
        &mut self,
        module: &Module,
        labels: Option<&LabelMap>,
        mut func: u32,
        mut pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        self.trace_on = ctx.trace_enabled();
        // Latch the per-run reduction-hook gate too (see the `Machine::reduce_on`
        // field docs): a host whose `reduce` is provably the default no-op declares
        // `wants_reduce() == false` and the driver then skips the per-instruction
        // vtable crossing entirely, paying one `bool` load instead. The default is
        // conservative `true`, so a pre-empting host keeps every charge.
        self.reduce_on = ctx.wants_reduce();
        // Latch the per-run in-machine BIF-dispatch gate too (see the
        // `Machine::bif_in_machine_on` field docs): a host that opts in declares
        // `wants_bif_in_machine() == true` and then receives the live machine and
        // the entry module on every BIF dispatch; the default `false` keeps the
        // plain `call_bif` path byte-identical and pays one `bool` load instead of an
        // extra vtable crossing.
        self.bif_in_machine_on = ctx.wants_bif_in_machine();
        // One `drive` call is one scheduling slice, so the reduction counter starts
        // here (the ERTS process counter, `c_p->fcalls`, is likewise reset when a
        // process is scheduled in; the host reads the count when the slice returns).
        self.reductions = 0;
        // The entry module's label index. A run entry that already resolved its
        // entry label hands its map in; otherwise it is built here (still exactly
        // one build per run).
        let built: LabelMap;
        let entry_labels: &LabelMap = match labels {
            Some(map) => map,
            None => {
                built = build_label_map(module);
                &built
            }
        };
        // Bind the starting context. `reset` left a fresh run in context `0`; a
        // resume arrives in the context the process blocked in.
        let mut entered: Option<Arc<ModuleContext>> = if self.current_context() == 0 {
            self.entry_literals = Some(module.literals.clone());
            self.set_literals(module.literals.clone());
            None
        } else {
            let handle = self
                .module_context(self.current_context())
                .ok_or_else(|| error::unsupported("run: unknown module context"))?;
            self.set_literals(handle.module.literals.clone());
            Some(handle)
        };
        loop {
            // The module/labels the next instruction is resolved against. Re-borrowed
            // per iteration: a context switch replaces `entered` below.
            let (active_module, active_labels): (&Module, &LabelMap) = match &entered {
                Some(handle) => (&handle.module, &handle.labels),
                None => (module, entry_labels),
            };
            // Record the machine's position as **this** instruction *before* pricing
            // it. The store is deliberately duplicated with the one
            // `step_instruction` does at its entry (`interpreter/dispatch.rs`): a
            // pre-emption below happens before `step_instruction` is reached, so
            // without this line the parked address would still be the *previous*
            // instruction — and `Machine::current_code()` is exactly the address a
            // host resumes at (`Machine::resume_outcome` takes the position
            // explicitly). Keeping it here is what makes an `Err` from the hook
            // leave the machine parked on the NOT-yet-executed instruction.
            self.set_current_code(func, pc);
            // Price the instruction **before** its body runs: the engine's own slice
            // counter (read back through `Machine::reductions`) and the host hook that
            // lets a **non-allocating** loop be pre-empted. `ensure_heap` — the only
            // other charge — is unreachable from such a loop, so this is the only point
            // a host is offered one reduction per instruction. Charging first is what
            // makes a pre-emption safe: an `Err` from `reduce` stops the run with the
            // instruction's effect NOT yet applied, so the host can resume at
            // `(func, pc)` and have it applied exactly once (C's `DISPATCH` likewise
            // checks/consumes `FCALLS` before the body and pre-empts without applying
            // it). The counter is bumped as part of the same charge, so it always
            // agrees with the hook.
            self.reductions = self.reductions.saturating_add(1);
            if self.reduce_on {
                ctx.reduce(1)?;
            }
            let step = match self.step_instruction_with_entry(
                module,
                active_module,
                active_labels,
                func,
                pc,
                ctx,
            ) {
                Ok(step) => step,
                // The uniform raise funnel's second half: `step_instruction` already
                // attached the frames when the raise happened in the run's **entry**
                // context (`0`, where the active module IS `module`). A raise that
                // happened inside a cross-module callee (a non-zero context) was left
                // for here, where the driver knows the true entry module — so the
                // caller-context frames are named against `module`, not the callee's.
                // A raise that already carries frames is returned unchanged, so the
                // two halves never double-derive them. `func`/`pc` still hold the
                // raising instruction's address.
                Err(err) => return Err(self.with_uncaught_frames(*err, Some(module), func, pc)),
            };
            match step {
                Step::Next => pc += 1,
                Step::Goto { func: f, pc: p } => {
                    func = f;
                    pc = p;
                }
                Step::Enter {
                    context,
                    func: f,
                    pc: p,
                } => {
                    if context != self.current_context() {
                        self.switch_context(context)?;
                        entered = match context {
                            0 => None,
                            id => Some(
                                self.module_context(id).ok_or_else(|| {
                                    error::unsupported("run: unknown module context")
                                })?,
                            ),
                        };
                    }
                    func = f;
                    pc = p;
                }
                Step::Wait { func: f, pc: p } => {
                    return Ok(RunOutcome::Wait { func: f, pc: p })
                }
                Step::Done => return self.finish().map(RunOutcome::Returned),
            }
        }
    }

    /// Follow the current frame's continuation as a control-flow [`Step`].
    ///
    /// A [`Continuation::Halt`] finishes the run ([`Step::Done`]); an address in the
    /// **current** module context is an intra-module [`Step::Goto`], and an address in
    /// another context is a [`Step::Enter`] the driver must switch for. The two are
    /// distinguished here (one integer compare) rather than at every call site.
    pub(crate) fn continuation_step(&self) -> Result<Step> {
        match self.continuation()? {
            Continuation::Halt => Ok(Step::Done),
            Continuation::Address {
                func,
                pc,
                context,
            } if context == self.current_context() => Ok(Step::Goto { func, pc }),
            Continuation::Address {
                func,
                pc,
                context,
            } => Ok(Step::Enter {
                context,
                func,
                pc,
            }),
        }
    }

    /// The number of reductions charged in the current (or most recently
    /// finished) scheduling slice.
    ///
    /// One `drive` call — a run, or a resume of a suspended process — is one
    /// scheduling slice, and the counter is zeroed at its start, so this is that
    /// slice's work in the unit the runtime prices pre-emption in. It is the
    /// **engine-side twin** of [`ExecutionContext::reduce`]: every charge bumps both,
    /// so the count always includes the last charge the hook saw — the one a host
    /// pre-empted on included — and a raising instruction (the charge precedes the
    /// body). The hook lets a host act *during* the slice; this accessor lets it
    /// observe *after* the slice returns (`RunOutcome::Wait` included) without
    /// keeping its own tally.
    ///
    /// The unit is the running tier's: the interpreter's dispatch loop charges one
    /// per **interpreted instruction**, while the native tiers' shared driver charges
    /// one per **driver-loop iteration** — a block entry, or the per-instruction step
    /// it takes for an address no compiled block covers — so the same work reports
    /// fewer units under the baseline JIT tier, and a natively linked local tail-call
    /// chain reports one per driver round-trip (`tests/jit_reductions.rs`). Every
    /// reported count is a real, exactly-once charge on that tier: never fabricated,
    /// never negative.
    pub fn reductions(&self) -> u64 {
        self.reductions
    }

    /// Zero the reduction counter, returning the value it held.
    ///
    /// [`Machine::drive`] already zeroes it at every slice entry, so a host only
    /// needs this to *re-base* a running tally (e.g. when it wants a window that
    /// spans several slices). The returned value is the count that was discarded, so
    /// a host that re-bases can still account for the work it saw.
    pub fn reset_reductions(&mut self) -> u64 {
        std::mem::replace(&mut self.reductions, 0)
    }
}

#[cfg(test)]
mod tests {
    //! The driver's `Step::Enter` contract: **every** way a `Step::Enter` reaches this
    //! loop must leave the machine's current module (its context and its literal
    //! binding) on the module the reported `(func, pc)` actually index. The three
    //! producers exercised here are a resolved cross-module `call_ext`, a
    //! cross-module `return` onto a caller-context continuation, and a raise that
    //! lands in a catch frame belonging to another module context. A wrong current
    //! module would resolve the next instruction against the wrong module's labels and
    //! literals — a silent correctness bug — so each test pins it explicitly.
    use std::sync::Arc;

    use beam_common::{BeamError, Result};
    use beam_loader::{Atom, HeapNeed, Instr, Label, Module, Operand};
    use beam_term::{make_small, Eterm, NIL};

    use crate::context::{ExecutionContext, MinimalContext};
    use crate::machine::test_support::{seam, Fixture};
    use crate::machine::{Machine, RunOutcome};

    /// The caller `m`: `run/0` allocates, `call_ext`s `c:<target>/0`, then receives
    /// (suspending on the empty mailbox).
    fn caller_then_receive(target: &str) -> (Module, Atom) {
        let mut f = Fixture::new("m");
        let call = f.import("c", target, 0);
        let run = f.add_function(
            "run",
            0,
            vec![
                Instr::Label(Label(100)),
                Instr::Allocate {
                    stack_need: HeapNeed::Words(0),
                    live: 0,
                },
                Instr::CallExt {
                    target: call,
                    arity: 0,
                },
                Instr::Deallocate(0),
                // The value the callee returned is ignored; the caller now blocks on a
                // receive, which is what lets the test observe which context it is in.
                Instr::Label(Label(101)),
                Instr::LoopRec {
                    fail: Label(102),
                    dst: Operand::X(0),
                },
                Instr::RemoveMessage,
                Instr::Return,
                Instr::Label(Label(102)),
                Instr::Wait { target: Label(101) },
            ],
        );
        f.add_export("run", 0, run);
        let name = f.atom("run");
        (f.finish(), name)
    }

    /// The callee `c`: `recv/0` (a bare receive that suspends on an empty mailbox) and
    /// `noop/0` (returns immediately).
    fn recv_callee() -> Module {
        let mut f = Fixture::new("c");
        let recv = f.add_function(
            "recv",
            0,
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
        );
        f.add_export("recv", 0, recv);
        let noop = f.add_function("noop", 0, vec![Instr::Label(Label(1)), Instr::Return]);
        f.add_export("noop", 0, noop);
        f.finish()
    }

    /// The caller `m`: a `try`/`catch` guarding a cross-module `call_ext c:boom/0`,
    /// whose handler returns the landed reason from `x(1)`.
    fn caller_guarding(target: &str) -> (Module, Atom) {
        let mut f = Fixture::new("m");
        let call = f.import("c", target, 0);
        let run = f.add_function(
            "run",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Allocate {
                    stack_need: HeapNeed::Words(1),
                    live: 0,
                },
                Instr::Try {
                    frame: Operand::Y(0),
                    fail: Label(20),
                },
                Instr::CallExt {
                    target: call,
                    arity: 0,
                },
                Instr::TryEnd {
                    frame: Operand::Y(0),
                },
                Instr::Deallocate(1),
                Instr::Return,
                Instr::Label(Label(20)),
                Instr::TryCase {
                    frame: Operand::Y(0),
                },
                Instr::Move {
                    src: Operand::X(1),
                    dst: Operand::X(0),
                },
                Instr::Deallocate(1),
                Instr::Return,
            ],
        );
        f.add_export("run", 0, run);
        let name = f.atom("run");
        (f.finish(), name)
    }

    /// The callee `c`: `boom/0` raises `42` with class `throw`.
    fn raising_callee() -> Module {
        let mut f = Fixture::new("c");
        let throw = f.atom("throw");
        let boom = f.add_function(
            "boom",
            0,
            vec![
                Instr::Label(Label(50)),
                Instr::Move {
                    src: Operand::Atom(throw),
                    dst: Operand::X(0),
                },
                Instr::Move {
                    src: Operand::Integer(42),
                    dst: Operand::X(1),
                },
                Instr::Move {
                    src: Operand::Nil,
                    dst: Operand::X(2),
                },
                Instr::RawRaise,
            ],
        );
        f.add_export("boom", 0, boom);
        f.finish()
    }

    /// Producer 1: a `call_ext` the host resolved through the seam runs the callee
    /// **in this machine**, so the machine's current module must be the callee's.
    #[test]
    fn a_resolved_call_ext_puts_the_machine_in_the_callee_module() {
        let (caller, run) = caller_then_receive("recv");
        let callee = Arc::new(recv_callee());
        let mut ctx = seam(vec![("c".to_string(), Arc::clone(&callee))]);
        let mut machine = Machine::new();

        let outcome = machine
            .run_export_outcome(&caller, run, 0, &[], &mut ctx)
            .expect("the callee suspends on its empty mailbox");
        assert!(
            matches!(outcome, RunOutcome::Wait { .. }),
            "the suspension is reported, got {outcome:?}"
        );
        assert_ne!(
            machine.current_context(),
            0,
            "the machine is inside the registered callee"
        );
        let current = machine
            .current_module()
            .expect("a registered callee has a module handle");
        assert!(
            Arc::ptr_eq(&current, &callee),
            "current_module must be the very handle the seam resolved with"
        );
    }

    /// Producer 2: a cross-module `return` onto the caller's continuation is an
    /// `Step::Enter { context: 0, .. }`, so the machine must switch **back** into the
    /// entry module. The caller then suspends in its own `receive`, which pins both
    /// the context and (via the resume address) the module the position belongs to.
    #[test]
    fn a_cross_module_return_switches_the_machine_back_into_the_caller_context() {
        let (caller, run) = caller_then_receive("noop");
        let callee = Arc::new(recv_callee());
        let mut ctx = seam(vec![("c".to_string(), Arc::clone(&callee))]);
        let mut machine = Machine::new();

        let outcome = machine
            .run_export_outcome(&caller, run, 0, &[], &mut ctx)
            .expect("the caller suspends after the callee returned");
        // The `wait` resumes at `Label(101)`, which is code index 4 of the caller's own
        // `run/0` — so the position is a caller address, not a callee one.
        assert_eq!(outcome, RunOutcome::Wait { func: 0, pc: 4 });
        assert_eq!(
            machine.current_context(),
            0,
            "the return switched the machine back to the entry context"
        );
        assert!(
            machine.current_module().is_none(),
            "context 0 is the borrowed entry module, reported as None"
        );
    }

    /// Producer 3: a raise inside the callee that lands in a catch frame belonging to
    /// the caller is `Step::Enter { context: 0, .. }` — the handler's `(func, pc)` are
    /// the caller's, so the machine must switch back before executing it.
    #[test]
    fn a_raise_landing_in_a_callers_catch_frame_switches_into_the_callers_context() {
        let (caller, run) = caller_guarding("boom");
        let callee = Arc::new(raising_callee());
        let mut ctx = seam(vec![("c".to_string(), Arc::clone(&callee))]);
        let mut machine = Machine::new();

        let outcome = machine
            .run_export_outcome(&caller, run, 0, &[], &mut ctx)
            .expect("the caller's try contains the callee's raise");
        assert_eq!(
            outcome,
            RunOutcome::Returned(make_small(42)),
            "the handler returned the landed reason"
        );
        assert_eq!(
            machine.current_context(),
            0,
            "the handler ran in the caller's context, not the callee's"
        );
        assert!(
            machine.current_module().is_none(),
            "context 0 is the borrowed entry module, reported as None"
        );
        assert_eq!(machine.catch_frame_depth(), 0, "the handler consumed its frame");
    }

    // -----------------------------------------------------------------------
    // The reduction charge is applied BEFORE the instruction's effect
    // -----------------------------------------------------------------------

    /// A host that wraps a [`MinimalContext`] — borrowing its pointer-stable region
    /// heap — and counts reduction charges and heap allocations.
    ///
    /// It is the smallest context that can *prove* an ordering: `alloc_words` is the
    /// countable, non-idempotent effect a `put_list` produces, and `reduce` is the
    /// pre-emption point. `wants_reduce()` is forced `true` (the engine must actually
    /// call the hook), unlike `MinimalContext`'s own `false`.
    struct ChargeCtx {
        inner: MinimalContext,
        /// The `n` of every `reduce` call, in order (the pre-empting one included).
        charges: Vec<u32>,
        /// Once set, `reduce` pre-empts on the charge past this many.
        budget: Option<u32>,
        /// How many `alloc_words` calls the host has served.
        allocations: u32,
    }

    impl ChargeCtx {
        fn with_budget(budget: u32) -> ChargeCtx {
            ChargeCtx {
                inner: MinimalContext::new(),
                charges: Vec::new(),
                budget: Some(budget),
                allocations: 0,
            }
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
            self.allocations += 1;
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
            if let Some(budget) = self.budget {
                if self.charges.len() as u32 > budget {
                    return Err(crate::error::unsupported("reduction budget exhausted"));
                }
            }
            Ok(())
        }

        fn wants_reduce(&self) -> bool {
            true
        }
    }

    /// The `.name` of a typed engine raise (the host's own reason, unsubstituted).
    fn reason_of(error: BeamError) -> String {
        match error {
            BeamError::Raise(raise) => raise
                .reason
                .as_atom()
                .expect("a typed engine error names its reason")
                .to_string(),
            other => panic!("expected a typed raise, got {other:?}"),
        }
    }

    /// `run/0`: two `put_list`s (each building one cons cell, i.e. one 2-word heap
    /// allocation), then `x0 = x1`, then `return`. Code index **2** is the second
    /// `put_list` — the instruction a budget of two charges pre-empts on. The X-file
    /// is `NIL` at reset, so the result is the two-element list `[NIL, NIL]`.
    fn two_conses_module() -> (Module, Atom) {
        let mut f = Fixture::new("m");
        let run = f.add_function(
            "run",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::PutList {
                    head: Operand::X(0),
                    tail: Operand::X(1),
                    dst: Operand::X(1),
                },
                Instr::PutList {
                    head: Operand::X(0),
                    tail: Operand::X(1),
                    dst: Operand::X(1),
                },
                Instr::Move {
                    src: Operand::X(1),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        );
        f.add_export("run", 0, run);
        let name = f.atom("run");
        (f.finish(), name)
    }

    /// **The pre-emption point is sound**: the reduction charge is applied **before**
    /// the instruction's body, so an `Err` from [`ExecutionContext::reduce`] leaves the
    /// machine parked on the **not-yet-executed** instruction with its effect
    /// un-applied; a [`Machine::resume_outcome`] at that address then applies it
    /// **exactly once**.
    ///
    /// Code index 2 is a `put_list` — a countable, non-idempotent effect (one 2-word
    /// cons-cell allocation). Under the unsound charge-after-execute order the
    /// pre-empted `put_list` would already have built a cons and the resume would build
    /// it again, so the returned list would be three conses long and the host would
    /// have served three allocations instead of two.
    #[test]
    fn a_pre_empted_instruction_is_unapplied_and_a_resume_applies_it_exactly_once() {
        let (module, run) = two_conses_module();
        let mut machine = Machine::new();
        let mut ctx = ChargeCtx::with_budget(2);

        // Budget 2: the charges for code index 0 (`label`) and index 1 (the first
        // `put_list`) are accepted; the third charge (code index 2, the second
        // `put_list`) pre-empts **before** that instruction runs.
        let err = machine
            .run_export_outcome(&module, run, 0, &[], &mut ctx)
            .expect_err("the third charge pre-empts the run");
        assert_eq!(
            reason_of(err),
            "unsupported_opcode: reduction budget exhausted",
            "the host's own typed error propagates unchanged"
        );
        assert_eq!(ctx.charges.len(), 3, "the pre-empting charge was offered");
        assert_eq!(
            machine.current_code(),
            Some((0, 2)),
            "the machine is parked on the NOT-yet-executed instruction"
        );
        assert_eq!(
            ctx.allocations, 1,
            "only the first `put_list` ran; the pre-empted one did NOT"
        );

        // Lift the budget and resume at the parked address: the pre-empted `put_list`
        // is applied exactly once, then the function finishes.
        ctx.budget = None;
        let outcome = machine
            .resume_outcome(&module, 0, 2, &mut ctx)
            .expect("the resume runs the rest of the function");
        let RunOutcome::Returned(list) = outcome else {
            panic!("the function returns a list, got {outcome:?}");
        };
        assert_eq!(
            ctx.allocations, 2,
            "the pre-empted `put_list` ran exactly once on the resume, not twice"
        );
        let elems = crate::terms::list_elements(list).expect("the result is a proper list");
        assert_eq!(
            elems,
            vec![NIL, NIL],
            "the result is EXACTLY two cons cells (a third would make it three elements)"
        );
    }
}
