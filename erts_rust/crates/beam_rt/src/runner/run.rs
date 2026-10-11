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
use std::time::Duration;

use beam_common::Result;
use beam_engine::context::RootSet;
use beam_engine::{ExecutionContext, Machine, RunOutcome as EngineRunOutcome};
use beam_process::{ProcessId, ProcessRunner, ProcessState, ProcessTable, RunOutcome};

use crate::context::gc;
use crate::context::RuntimeContext;
use crate::runtime::Runtime;

use super::budget::{BudgetContext, BudgetProbe};
use super::entry::{ProcessEntry, RunnerState};
use super::perf::record_baseline_code;
use super::reason::{exit_reason, normal};
use super::tier::select_entry_tier;

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

/// Publishes a **parked** machine's whole live root set into `pid`'s process root set.
///
/// A suspended process's machine lives in [`RunnerState::engines`], but a machine's registers
/// are only a *copy* of the process's root set. A collection driven while the process is parked
/// — [`Process::deliver`](beam_process::Process::deliver)'s exit cascade is **not** gated by
/// [`Runtime::gc_process_enabled`](crate::runtime::Runtime) — relocates the process's heap
/// against its own `regs`, so the parked machine must have published them first; otherwise the
/// collector runs against a partial root set and leaves a stale word in the machine's `x`
/// ([`crate::context::gc`] documents the hand-off).
///
/// The machine's **whole** root set is published — every x register, the whole Erlang stack
/// **and the whole FR file** ([`gc::publish_roots_full`](crate::context::gc::publish_roots_full)).
/// [`RootSet::ALL_LIVE`] is the conservative choice for the x file: the parked machine's live
/// count is not exposed, over-approximating a copying collector's root set is always *safe* (it
/// can only keep a term alive, never free a live one), and the FR file holds `Eterm` term slots
/// at Milestone 1 so it is a root like any other. A `pid` with no process in `table` has nothing
/// to publish into; the machine is left as it is.
fn publish_parked_roots(table: &mut ProcessTable, pid: ProcessId, machine: &mut Machine) {
    let Some(process) = table.get_mut(pid) else {
        return;
    };
    machine.with_roots(RootSet::ALL_LIVE, |roots| {
        gc::publish_roots_full(process, &roots)
    });
}

/// Re-syncs a machine about to **resume** from the process's authoritative root set.
///
/// The twin of [`publish_parked_roots`]: while the process was parked, a delivery-driven
/// collection may have relocated its heap and rewritten the process's `regs` in place, leaving
/// the parked machine's own `x`/`stack` pointing into the abandoned space. The process's root
/// set — not the machine's stale copy — is therefore the truth a resume must run from; the
/// context method reconciles them and halts the process's collection if their slices no longer
/// match (see [`RuntimeContext::resync_parked_roots`]).
fn resume_from_process_roots(ctx: &mut RuntimeContext<'_>, pid: ProcessId, machine: &mut Machine) {
    ctx.resync_parked_roots(pid, machine);
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
    /// # Execution tier
    ///
    /// The tier ([`Tier`](beam_engine::Tier)) is selected **once**, on the process's first
    /// entry (`super::tier`), and remembered on its engine (`ProcessEngine::tier`); every
    /// later slice — a pre-emption or a `receive` park — resumes on that **same** tier
    /// through [`Machine::resume_outcome_with_tier`](beam_engine::Machine::resume_outcome_with_tier),
    /// so a native-tier process is never silently demoted to the interpreter mid-life. A
    /// process whose remembered tier is [`Tier::Interpreter`](beam_engine::Tier::Interpreter)
    /// behaves exactly as it always did (the dispatcher delegates to the plain
    /// [`Machine::resume_outcome`](beam_engine::Machine::resume_outcome)).
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

        // The slice's execution tier: selected at the process's **first** entry (resolved
        // *before* the context borrows the runtime, because the switch-safety predicate reads
        // `Runtime`'s `registry`/`nifs`, which `for_process_with_engines` takes mutably below)
        // and then **remembered on the engine** (`ProcessEngine::tier`), so a resumed process —
        // one that pre-empted on the reduction budget, or parked in a `receive` — re-enters on
        // that same tier instead of falling back to the interpreter. A fun entry whose local
        // target is only resolved by that first run reports no module here, and is
        // conservatively interpreted.
        let tier = match engine.parked {
            Some(_) => engine.tier,
            None => {
                let mode = self.state.jit_mode();
                let selected = select_entry_tier(self.runtime, self.state, &engine.entry, mode);
                engine.tier = selected;
                selected
            }
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
            //
            // A **fun-bodied** process whose fun is a `.beam` **local** (`make_fun3`)
            // closure resolves its launch target **once**, on this first run: the defining
            // module, the lambda's entry label and the combined (free ++ args) arguments.
            // It is entered at that label on *this process's* machine (below), exactly as an
            // export entry is, so a `receive` inside the closure — or inside a cross-module
            // callee it calls — reports `RunOutcome::Wait` and the process can park instead
            // of folding the suspension into the typed `blocked()` error that
            // `ExecutionContext::apply_fun` reports (see [`ProcessEntry::Fun`]).
            //
            // The two fun kinds with **no** local label — an external `fun M:F/A` and the
            // `-eval` evaluator's own closure — stay `None`, and are applied synchronously
            // through `budget_ctx.apply_fun` below (the local resolver reports `Ok(None)`
            // for both, and its error — a non-fun, or a local closure no loaded module
            // defines — becomes this process's exit reason exactly as it would have).
            let resolved = match &mut engine.entry {
                ProcessEntry::Fun { fun, target } if target.is_none() => ctx
                    .local_fun_target(*fun, &engine.args)
                    .map(|resolved| *target = resolved),
                _ => Ok(()),
            };
            // The process's call site is the module whose code it is about to run — the
            // export's module, or the module a fun entry resolved to. A fun entry with no
            // local label keeps `None` (it is applied as a value, not entered as code).
            if let Some(module) = engine.entry.entry_module() {
                ctx.enter_module(module);
            }
            // A resume continues a machine an earlier slice parked. While it was parked a
            // delivery-driven collection may have relocated this process's heap, so re-sync
            // the machine from the process's authoritative root set before running it (see
            // `resume_from_process_roots`).
            if engine.parked.is_some() {
                resume_from_process_roots(&mut ctx, pid, &mut engine.machine);
            }
            // The slice's reduction budget, charged at the machine's heap-admission points
            // (see the `run` docs).
            let mut probe = BudgetProbe::new(budget);
            let mut budget_ctx = BudgetContext {
                inner: &mut ctx,
                probe: &mut probe,
            };
            let outcome = match resolved {
                Err(error) => Err(error),
                Ok(()) => match engine.parked {
                    // A resume continues the SAME machine: the registers, the Erlang stack and
                    // the frames of the suspended `receive` must survive, so this deliberately
                    // does not reset (see `Machine::resume_outcome_with_tier`). It also
                    // continues on the **same tier** the process was selected for (`tier`,
                    // remembered on the engine), so a native-tier process is not demoted to the
                    // interpreter after its first slice. The entry module is the
                    // module the suspended `(func, pc)` are named against — the export's own
                    // module, or the module a fun entry resolved to. A parked fun entry with
                    // **no** resolved target is unreachable (it is applied synchronously and
                    // never returns a resume address) and is reported as a typed capability
                    // gap rather than run as a wrong (or empty) function.
                    Some((func, pc)) => match engine.entry.entry_module() {
                        Some(module) => engine.machine.resume_outcome_with_tier(
                            module,
                            func,
                            pc,
                            &mut budget_ctx,
                            tier,
                        ),
                        None => Err(crate::error::unsupported(
                            "resume of a fun-bodied process with no local entry",
                        )),
                    },
                    None => match &engine.entry {
                        ProcessEntry::Export {
                            module,
                            function,
                            arity,
                        } => match module.export(*function, *arity) {
                            // Entered at the export's own label on the tier chosen above.
                            //
                            // The tier is the baseline JIT only when the module is
                            // *provably* free of a module-context switch (`Step::Enter`):
                            // a `call_ext*` the host resolves through
                            // `ExecutionContext::resolve_external_call` (see
                            // `src/context/mod.rs`) is entered in this same machine as a
                            // `Step::Enter`. The baseline-JIT driver **does** implement that
                            // in-machine module-context switch today — it is the `ENTER`
                            // block-ABI word, resolved and re-entered in native code (see
                            // `crates/beam_engine/src/jit/CONTEXT.md`) — so the predicate
                            // below is a deliberately conservative *tier-selection* policy,
                            // not an engine limitation. `tier` is `Tier::Interpreter` for
                            // every module the predicate does not prove safe; the predicate
                            // and its exact conditions live in `super::tier`. The resume path
                            // above re-enters on the tier this process was selected for
                            // (remembered in `ProcessEngine::tier`), so both halves of a
                            // parked-and-resumed process stay tier-equivalent.
                            //
                            // The label entry stays the equivalence-preserving route to the
                            // export: `run_label_outcome_with_tier` calls `Machine::enter_run`,
                            // so the run's `initial_call`/`current_function` are recorded and
                            // `process_info/2`'s execution-position items can be answered.
                            Some(label) => engine.machine.run_label_outcome_with_tier(
                                module,
                                label,
                                *arity,
                                &engine.args,
                                &mut budget_ctx,
                                tier,
                            ),
                            // An export the module does not define stays the engine's own typed
                            // error, byte for byte (`Machine::run_export_outcome`'s). Unreachable
                            // for a spawned entry (`Runtime::spawn_process` resolves the export
                            // first), and never a fabricated result.
                            None => Err(crate::error::unsupported(
                                "run_export: export not found",
                            )),
                        },
                        // A fun entry: a resolved **local** closure is entered at its own label
                        // on this process's machine — the same entry an export takes, so its
                        // `receive` can park the process (see above). A fun with no local label
                        // is applied synchronously: `apply_fun` routes an `-eval` closure back
                        // into the evaluator and an external `fun M:F/A` into an ordinary
                        // dynamic call — neither is re-implemented here. A returned value is the
                        // fun's result, so it records exactly like an export's.
                        ProcessEntry::Fun { fun, target } => match target {
                            Some(target) => engine.machine.run_label_outcome_with_tier(
                                &target.module,
                                target.label,
                                target.arity,
                                &target.args,
                                &mut budget_ctx,
                                tier,
                            ),
                            None => budget_ctx
                                .apply_fun(*fun, &engine.args)
                                .map(EngineRunOutcome::Returned),
                        },
                    },
                },
            };            // The wrapper's borrow of the probe ends here (the machine is no longer
            // running), so the slice's accounting is readable.
            drop(budget_ctx);
            let used = probe.used();
            let exhausted = probe.is_exhausted();
            (outcome, used, exhausted)
        };

        // Credit every reduction unit this slice charged to the node-global counter behind
        // `erlang:statistics(reductions)` (see `Runtime::note_reductions`). The charge site is
        // the slice's `BudgetProbe`, so this is exactly the work the machine did — the same
        // `used` the exhausted branch below records on the process.
        self.runtime.note_reductions(u64::from(used));

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
            // Publish the machine's roots before the engine is parked: a collection driven
            // while this process sits in `RunnerState.engines` must be able to relocate the
            // heap against the machine's live terms (see `publish_parked_roots`).
            publish_parked_roots(table, pid, &mut engine.machine);
            // A `garbage_collect` request (see `Runtime::request_gc`) that no heap-admission
            // point served is honoured **here**: the roots are just published and the
            // machine's float-register file is provably dead (the scan above pinned the pid
            // otherwise), so this is a legitimate collection site.
            self.runtime.serve_gc_request(table, pid);
            self.state.engines.insert(pid, engine);
            return Ok(RunOutcome::Yield);
        }

        match outcome {
            Ok(EngineRunOutcome::Returned(value)) => {
                // `x(0)`, not an exit reason: copy it onto the runtime's pointer-stable
                // arena (so it outlives the process, whose own heap dies with it) and
                // record it, then terminate the process normally.
                //
                // The machine is still in scope and holds the off-heap payload handles for
                // the refc binaries it built (a `bs_init_writable` accumulator, or any
                // `bs_create_bin` result above the on-heap limit); a binary the process
                // **received** from another process carries its handles on this pid's scan
                // state instead. The copy's source payloads are the union of the two
                // (`Runtime::source_payloads`), so both resolve instead of refusing the term
                // with the empty source table the simple form uses (see
                // [`Runtime::stabilize_term_with`](crate::runtime::Runtime::stabilize_term_with)).
                let value = {
                    let payloads =
                        self.runtime.source_payloads(pid, engine.machine.refc_binaries());
                    self.runtime.stabilize_term_with(value, &payloads)?
                };
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
                // waiting for a message.
                //
                // The deadline is recovered through `Machine::wait_deadline`, which is
                // **cross-module aware**: a machine parked inside a `call_ext*` callee the
                // host resolved through `ExecutionContext::resolve_external_call` sits at a
                // `wait_timeout` in the **callee's** instruction stream, so it reads that
                // callee's own stream and falls back to the entry module (the export's own,
                // or the module a fun entry resolved to) for a same-module park. Reading the
                // entry module alone would lose the deadline and park the process forever.
                //
                // `delay_ms` is `Some(d)` exactly when the delay is statically readable (a
                // bare `wait` — `after infinity` — and an unreadable delay both arm nothing),
                // and `WaitDeadline::resume` is the `timeout` continuation a fired timer
                // re-enters.
                engine.parked_timeout = None;
                if let Some(module) = engine.entry.entry_module() {
                    if let Some(deadline) = engine.machine.wait_deadline(module) {
                        if let Some(delay_ms) = deadline.delay_ms {
                            let handle = self.runtime.schedule_receive_timeout(
                                pid,
                                Duration::from_millis(delay_ms),
                                deadline.resume.0,
                                deadline.resume.1,
                            );
                            engine.parked_timeout = Some(handle);
                        }
                    }
                }
                if let Some(process) = table.get_mut(pid) {
                    process.set_state(ProcessState::Waiting);
                }
                // Publish the machine's roots before the engine is parked: a delivery-driven
                // collection (`Process::deliver`'s exit cascade, ungated by
                // `gc_process_enabled`) may relocate this process's heap while it waits (see
                // `publish_parked_roots`).
                publish_parked_roots(table, pid, &mut engine.machine);
                // ...and serve a pending `garbage_collect` request (see
                // `Runtime::request_gc`), which is why a process that asks for a collection
                // and then blocks is still collected.
                self.runtime.serve_gc_request(table, pid);
                self.state.engines.insert(pid, engine);
                Ok(RunOutcome::Wait)
            }
            Err(error) => {
                let reason = exit_reason(self.runtime, &error);
                // The reason may be a term the failing process built on **its own** heap,
                // which the collector may move or drop the moment the process terminates:
                // copy it onto the runtime's pointer-stable arena before it is recorded. A
                // reason that *genuinely* cannot be copied is the honest `error:system_limit`
                // (it is propagated, never stored dangling), while the documented off-heap
                // capability gap keeps the original term and freezes the heaps — see
                // `Runtime::try_stabilize_escaped_with`.
                //
                // The machine is still in scope and owns the off-heap payload handles for the
                // refc binaries it built, while a binary the process **received** from another
                // process carries its handles on this pid's scan state; the copy's source
                // payloads are the union of the two (`Runtime::source_payloads`), so a reason
                // like `exit(<<big/binary>>)` resolves either kind rather than tripping the valve
                // over a payload the empty source table could not resolve.
                let reason = {
                    let payloads =
                        self.runtime.source_payloads(pid, engine.machine.refc_binaries());
                    self.runtime.try_stabilize_escaped_with(reason, &payloads)?
                };
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
