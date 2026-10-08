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

//! The [`ProcessRunner`] implementation: **one reduction slice of one scheduled process**.
//!
//! [`RuntimeRunner`] ties the runtime (BIF dispatch, atoms, heaps) to the [`RunnerState`]
//! moved out of it, and the process table arrives per call from the scheduler. `run` is the
//! whole slice: settle a parked receive's deadline, run or resume the process's machine
//! under the slice's reduction budget, and record exactly one outcome — a recorded
//! `x(0)` + `normal` exit, a parked wait, a pre-emptive yield, or the raise's reason as the
//! exit reason.

use std::sync::Arc;

use beam_common::Result;
use beam_engine::{ExecutionContext, RunOutcome as EngineRunOutcome, Tier};
use beam_process::{ProcessId, ProcessRunner, ProcessState, ProcessTable, RunOutcome};

use crate::context::RuntimeContext;
use crate::runtime::Runtime;

use super::budget::{BudgetContext, BudgetProbe};
use super::entry::{ProcessEntry, RunnerState};
use super::perf::record_baseline_code;
use super::reason::{exit_reason, normal, parked_receive_timeout};

/// The `beam_process::ProcessRunner` the runtime hands to the scheduler.
///
/// It borrows the runtime (for BIF dispatch, atoms and heaps) and the
/// [`RunnerState`] moved out of it; the process table arrives per call, supplied by the
/// scheduler.
pub(crate) struct RuntimeRunner<'a> {
    /// The runtime the process's code runs on.
    runtime: &'a mut Runtime,
    /// The engines and outcomes (moved out of the runtime for the pass).
    state: &'a mut RunnerState,
}

impl<'a> RuntimeRunner<'a> {
    /// A runner over `runtime` and the moved-out `state`.
    pub(crate) fn new(runtime: &'a mut Runtime, state: &'a mut RunnerState) -> RuntimeRunner<'a> {
        RuntimeRunner { runtime, state }
    }
}

impl ProcessRunner for RuntimeRunner<'_> {
    /// Runs one slice of process `pid`: its first entry, or the resume of the `receive`
    /// that suspended it.
    ///
    /// # Reduction accounting and pre-emption
    ///
    /// `budget` is the slice's **reduction budget** (the C `CONTEXT_REDS`); the runner owns
    /// the reduction count (the scheduler never counts — see the [`ProcessRunner`]
    /// contract). It is charged by [`BudgetProbe`] at the machine's heap-admission points
    /// — every `test_heap`/`allocate_heap` instruction the executing machine runs reaches
    /// [`ExecutionContext::ensure_heap`](beam_engine::ExecutionContext::ensure_heap) once —
    /// and the slice **ends in [`Yield`](beam_process::RunOutcome::Yield)** once the budget
    /// is spent, so a CPU-bound process is pre-empted and the scheduler requeues it instead
    /// of letting it monopolize the run queue.
    ///
    /// The yield is **resumption-exact**: the machine is parked at
    /// [`Machine::current_code`](beam_engine::Machine::current_code) — the very instruction
    /// whose `ensure_heap` ran out (the engine records it as the first statement of
    /// `step_instruction`, and re-running `ensure_heap` from outside is a no-op) — so the
    /// next slice resumes *at* that instruction with the registers, the stack and the frames
    /// intact, exactly as a `receive` resume does. No result is fabricated and no work is
    /// repeated.
    ///
    /// The budget is only charged from the **slice's own** machine: the wrapper is the
    /// context of that machine alone, and every machine a BIF reaches (an external module
    /// call, a fun apply) is built inside `RuntimeContext` with the plain context, so a
    /// nested run can never consume the budget nor be mistaken for the resumable machine.
    /// A loop the compiler emits **without** a heap test (`spin/1`-style: no allocation, so
    /// no `test_heap`) has no admission point and therefore runs to its next
    /// `return`/`wait` without being pre-empted — the honest limit, reported as such.
    ///
    /// # Errors
    ///
    /// * a typed diagnostic when `pid` has no registered engine (a pid the runtime did
    ///   not spawn cannot be run);
    /// * the process table's errors while a `send` delivered into a woken mailbox.
    ///
    /// A *raise inside the process* is **not** an error here: it becomes
    /// [`RunOutcome::Exit`] carrying the raised reason, exactly as the BEAM terminates a
    /// process with the reason it raised.
    fn run(&mut self, pid: ProcessId, table: &mut ProcessTable, budget: u32) -> Result<RunOutcome> {
        // Take the engine out of the map: it is borrowed mutably for the whole slice while
        // `self.runtime` is borrowed by the context, and a `HashMap` cannot hand out a
        // `&mut` value while its own container is still reachable. It goes back in below
        // unless the process finished.
        let Some(mut engine) = self.state.engines.remove(&pid) else {
            return Err(crate::error::unknown_process_entry(pid));
        };

        // Settle a parked receive's deadline **before** the context borrows the runtime:
        // a deadline that fired resumes the process at the timed-out path, while a
        // message-wake cancels the deadline so the receive cannot also fire its timeout.
        if engine.parked.is_some() {
            match self.runtime.take_timer_resume(pid) {
                Some(address) => {
                    // The deadline won the race: resume at the `timeout` instruction with
                    // no message delivered (the mailbox is untouched, so the `after` clause
                    // runs).
                    engine.parked = Some(address);
                    engine.parked_timeout = None;
                }
                None => {
                    // A message woke the receiver first: drop the deadline. Cancelling one
                    // that already fired is a no-op, which is the honest read of the race.
                    if let Some(handle) = engine.parked_timeout.take() {
                        self.runtime.cancel_receive_timeout(handle);
                    }
                }
            }
        }

        // The **first** entry of a module-export process is the only run that goes through the
        // tier-aware entry below (a resume has no tier entry), so it is the one site that can
        // snapshot the module's baseline artifact for the `+JPperf` map. Gated on the flag
        // first: a disabled `+JPperf` (the default) must cost nothing at all.
        let first_entry_export = match &engine.entry {
            ProcessEntry::Export { module, .. }
                if engine.parked.is_none() && self.runtime.perf_map().config().map_enabled() =>
            {
                Some(Arc::clone(module))
            }
            _ => None,
        };

        let (outcome, used, exhausted) = {
            // The context also gets the **engines** of this pass (`self.state`): it drives
            // *this* process's machine (taken out of the map above), but a
            // `process_info/2` execution-position item must be able to read every *other*
            // scheduled process's position (see `crate::native::memory`).
            let mut ctx =
                RuntimeContext::for_process_with_engines(&mut *self.runtime, table, pid, self.state);
            // A BIF reached from this code must know its **call site**: the oracle derives
            // `erlang:load_nif/2`'s owning module from the call instruction, so a scheduled
            // process sets it exactly as `RuntimeContext::run_export` sets it for the
            // single-pass path (see [`RuntimeContext::enter_module`]).
            if let ProcessEntry::Export { module, .. } = &engine.entry {
                ctx.enter_module(module);
            }
            // The slice's reduction budget, charged at the machine's heap-admission points
            // (see the `run` docs).
            let mut probe = BudgetProbe::new(budget);
            let mut budget_ctx = BudgetContext {
                inner: &mut ctx,
                probe: &mut probe,
            };
            let outcome = match engine.parked {
                // A resume continues the SAME machine: the registers, the Erlang stack and
                // the frames of the suspended `receive` must survive, so this deliberately
                // does not reset (see `Machine::resume_outcome`). Only a module-export
                // process can park, so a parked fun entry is unreachable — it is reported
                // as a typed capability gap rather than run as a wrong (or empty) function.
                Some((func, pc)) => match &engine.entry {
                    ProcessEntry::Export { module, .. } => {
                        engine.machine.resume_outcome(module, func, pc, &mut budget_ctx)
                    }
                    ProcessEntry::Fun { .. } => Err(crate::error::unsupported(
                        "resume of a fun-bodied process (a fun entry never suspends)",
                    )),
                },
                None => match &engine.entry {
                    ProcessEntry::Export {
                        module,
                        function,
                        arity,
                    } => match module.export(*function, *arity) {
                        // Entered at the export's own label on the **baseline** tier. The
                        // label entry is the equivalence-preserving route to it: the export
                        // path only resolves this same label and calls the label path, but
                        // the baseline *export* driver (`beam_engine`'s `run_native_outcome`)
                        // never calls `Machine::enter_run`, so the run's `entry_call` stays
                        // unset and `process_info/2`'s `initial_call`/`current_function` could
                        // not be answered, whereas the baseline *label* driver records it.
                        // That is a `beam_engine` gap this crate may not patch (workaround).
                        Some(label) => engine.machine.run_label_outcome_with_tier(
                            module,
                            label,
                            *arity,
                            &engine.args,
                            &mut budget_ctx,
                            Tier::BaselineJit,
                        ),
                        // An export the module does not define stays the engine's own typed
                        // error, byte for byte (`Machine::run_export_outcome`'s). Unreachable
                        // for a spawned entry (`Runtime::spawn_process` resolves the export
                        // first), and never a fabricated result.
                        None => Err(crate::error::unsupported(
                            "run_export: export not found",
                        )),
                    },
                    // A fun entry is applied synchronously: `apply_fun` routes an `-eval`
                    // closure back into the evaluator and a `.beam` fun into the engine's
                    // label entry — the evaluator is never re-implemented here. A returned
                    // value is the fun's result, so it records exactly like an export's.
                    ProcessEntry::Fun { fun } => budget_ctx
                        .apply_fun(*fun, &engine.args)
                        .map(EngineRunOutcome::Returned),
                },
            };
            // The wrapper's borrow of the probe ends here (the machine is no longer
            // running), so the slice's accounting is readable.
            drop(budget_ctx);
            let used = probe.used();
            let exhausted = probe.is_exhausted();
            (outcome, used, exhausted)
        };

        // The **float-register safety valve**, checked while the machine is no longer
        // running: `Machine::fr` holds *terms* at Milestone 1 and is not part of the root set
        // `ensure_heap` hands out, so a heap move while a float register is live could
        // silently corrupt float code. The module-level float guard
        // (`context::gc::module_uses_floats`) already refuses to collect over a module that
        // contains a float instruction; this catches the residual case — a float register
        // left set by *another* module's code (a callee) while this process runs float-free
        // code — by pinning the process permanently. This crate can neither read nor clear
        // `Machine::fr` from the admission point, so pinning is the only safe reaction (see
        // `runtime::gc`).
        if (0..beam_engine::machine::MAX_FLOAT_REGISTER)
            .any(|index| engine.machine.get_fr(index).is_some_and(|fr| fr != beam_term::NIL))
        {
            self.runtime.pin_process(pid);
        }
        // The `+JPperf` map (contract A.8) must describe the **real** native code this runtime
        // can run, never a fabricated range: the module's baseline artifact is that source, so
        // its cached `[start, size)` regions are snapshotted here -- once per first entry, not
        // per slice. Diagnostic only: `record_baseline_code` records, never propagates.
        if let Some(module) = first_entry_export.as_deref() {
            record_baseline_code(self.runtime, module);
        }

        // Pre-emption: the budget ran out at a heap-admission point of this slicing
        // machine. Park it at that very instruction and hand the process back to the
        // scheduler, which requeues it (`slice_end`'s `Yield` arm) so a second runnable
        // process gets the next slice. Nothing is recorded as a result/exit and the
        // process stays `Runnable` — it has neither returned nor blocked.
        if exhausted {
            let Some(address) = engine.machine.current_code() else {
                // Unreachable: only `ensure_heap` (reached from `test_heap`/
                // `allocate_heap`, which record their address first) charges the budget.
                // Reported as a typed error rather than parked at a guessed address — no
                // wrong result, no hang.
                return Err(crate::error::unsupported(
                    "preemption: the exhausted machine has no resumable instruction address",
                ));
            };
            if let Some(process) = table.get_mut(pid) {
                // The runner owns the reduction count: record what this slice spent so
                // `process_info(_, reductions)` reports real work.
                process.reset_reductions();
                process.bump_reductions(u64::from(used));
            }
            engine.parked = Some(address);
            // A parked receive's deadline was settled above; a yield has no deadline of its
            // own, so nothing is left armed here (a stale one would fire mid-run later).
            engine.parked_timeout = None;
            self.state.engines.insert(pid, engine);
            return Ok(RunOutcome::Yield);
        }

        match outcome {
            Ok(EngineRunOutcome::Returned(value)) => {
                // `x(0)`, not an exit reason: copy it onto the runtime's pointer-stable
                // arena (so it outlives the process, whose own heap dies with it) and
                // record it, then terminate the process normally.
                let value = self.runtime.stabilize_term(value)?;
                self.state.results.insert(pid, value);
                Ok(RunOutcome::Exit(normal(self.runtime)))
            }
            Ok(EngineRunOutcome::Wait { func, pc }) => {
                // The runner owns the state transition (the scheduler only takes the
                // process off the ready queue): a blocked process is `Waiting` until a
                // delivery wakes it, which is exactly what makes a later send resume it.
                engine.parked = Some((func, pc));
                // A `wait_timeout` with a statically known positive delay arms a deadline
                // now: when it fires the process resumes at the timed-out path instead of
                // waiting for a message (see `parked_receive_timeout`).
                engine.parked_timeout = None;
                if let ProcessEntry::Export { module, .. } = &engine.entry {
                    if let Some((delay, (resume_func, resume_pc))) =
                        parked_receive_timeout(&engine.machine, module, func, pc)
                    {
                        let handle = self.runtime.schedule_receive_timeout(
                            pid,
                            delay,
                            resume_func,
                            resume_pc,
                        );
                        engine.parked_timeout = Some(handle);
                    }
                }
                if let Some(process) = table.get_mut(pid) {
                    process.set_state(ProcessState::Waiting);
                }
                self.state.engines.insert(pid, engine);
                Ok(RunOutcome::Wait)
            }
            Err(error) => {
                let reason = exit_reason(self.runtime, &error);
                // The reason may be a term the failing process built on **its own** heap,
                // which the collector may move or drop the moment the process terminates:
                // copy it onto the runtime's pointer-stable arena before it is recorded (see
                // `Runtime::stabilize_escaped`).
                let reason = self.runtime.stabilize_escaped(reason);
                self.state.exits.insert(pid, reason);
                // Keep the original error as well: the reason *term* above is what the exit
                // propagates, but only the error carries the raise class and the
                // `unsupported` classification the CLI keys off (see `RunnerState::errors`).
                self.state.errors.insert(pid, error);
                Ok(RunOutcome::Exit(reason))
            }
        }
    }
}
