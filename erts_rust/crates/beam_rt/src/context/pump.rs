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

//! The nested-run pump: running a module export as a **scheduled process**.
//!
//! [`RuntimeContext::run_export`] drives one export on a fresh [`Machine`], and its blocking
//! twins ([`RuntimeContext::run_export_blocking`] / [`RuntimeContext::run_label_blocking`])
//! pump the scheduler while a cross-module callee is suspended in a `receive`, so the callee
//! **suspends and resumes** instead of folding into the typed [`beam_engine::blocked`] gap.
//! [`pump_until_returned`] is the shared bounded loop and [`BLOCKING_PUMP_DEADLINE`] its
//! wall-clock safety cap.

use std::time::{Duration, Instant};

use beam_common::Result;
use beam_engine::machine::WaitDeadline;
use beam_engine::{Machine, RunOutcome};
use beam_loader::{Atom, Label, Module};
use beam_term::Eterm;

use crate::error;

use super::runtime_context::RuntimeContext;

/// How many scheduler slices [`RuntimeContext::run_export`] drives between resumes of a
/// cross-module callee suspended in a `receive`.
///
/// This is the external-call counterpart of the `-eval` evaluator's receive pump
/// ([`crate::eval`]'s `RECEIVE_PUMP_SLICES`): a bounded step, so a call that blocks has the
/// child it waits on run and re-scanned, and a pump that reports nothing ran ends the wait
/// with the honest typed [`beam_engine::blocked`] gap instead of spinning. Each round is a
/// [`Runtime::pump_scheduler_boot`](crate::runtime::Runtime::pump_scheduler_boot) pass — the
/// **imminent-only** sleep policy — so an idle round whose only pending deadline is far out
/// yields immediately rather than stalling the caller (see [`RuntimeContext::pump_once`]).
const RUN_EXPORT_PUMP_SLICES: u64 = 64;

/// The wall-clock safety cap on a nested blocking pump (see [`pump_until_returned`]).
///
/// [`RuntimeContext::run_export_blocking`] and [`RuntimeContext::run_label_blocking`] drive the
/// scheduler while a cross-module callee is suspended in a `receive` (one bounded round of
/// [`RUN_EXPORT_PUMP_SLICES`] slices per wait). The loop's ordinary exits are the awaited run's
/// own `return` and a pump that reports **nothing ran** (nothing could have changed the mailbox,
/// so the wait is honest), and this cap is checked *between* rounds — it bounds the number of
/// rounds, never the work inside one. It exists for the one shape neither ordinary exit covers:
/// a peer that stays **runnable forever**, which reports progress on every round, so the drive
/// would spin without end. Like the boot drive's `DRIVE_DEADLINE`, it turns that into a typed
/// diagnostic instead of a hang.
///
/// # Sizing: the heaviest legitimate workload on this route
///
/// That workload is an **in-process compile** of a real stdlib module — what
/// `beam_rt -noshell -eval '<compile:file(Src, [no_spawn_compiler_process, deterministic,
/// no_docs, {outdir, D}])>'` does, and what the `in_process_compile` test below drives. It is a
/// genuine **pump loop**, not one burst: each `receive` the compiler makes (its `epp` and
/// file-server round trips, its `io` calls) parks the machine and pumps the scheduler between
/// them, so a cap sized for a small module truncates a large one. Measured `user` time on this
/// host, debug runtime, `lib/stdlib/src/` with the option list above: `dets.erl` ≈ 100 s,
/// `erl_eval.erl` ≈ 83 s, `erl_lint.erl` ≈ 107 s, `shell.erl` ≈ 81 s, `qlc.erl` ≈ 155 s (wall
/// up to ≈ 188 s when the host is loaded — the compile is CPU-bound, so `user` is the stable
/// figure and the oracle's identical-option compile of each takes ≈ 1–2 s).
///
/// Fifteen minutes is ≈ 4.8× the largest measured figure: it clears the whole installed corpus
/// with headroom for a slower or busier host, while still reporting a genuinely infinite pump in
/// bounded time rather than looking like a hang.
///
/// # Why this total cap is the *only* bound
///
/// A forward-progress trip is tempting — the failure mode above is a peer burning a core
/// forever — and [`Machine::reductions`](beam_engine::Machine::reductions) looks like its
/// signal: it counts the interpreted instructions the awaited run charged in its last resume, a
/// legitimate compile charges thousands per round, and a run that merely re-scans its mailbox
/// while a peer spins charges a handful. It is nevertheless **unsound here** and deliberately
/// unused. The awaited machine's own counter says nothing about the process it waits *on*, so
/// "the awaited run re-parks with ~0 reductions" is equally the signature of a **legitimate**
/// busy peer — a `gen_server:call` whose server computes for minutes, a slow disk, a calibrated
/// workload — and tripping on it would replace a result the oracle answers with this runtime's
/// own error. No progress-gated bound can separate those two shapes, and one that also required
/// the pump to report nothing ran would never fire at all: that *is* the loop's `Ok(None)` exit.
/// So the total cap is the required floor, and it is the simplest sound design.
pub(super) const BLOCKING_PUMP_DEADLINE: Duration = Duration::from_secs(900);

/// The longest [`RuntimeContext::pump_once`] sleeps between two scheduler rounds while it
/// waits a parked `receive … after T` out (see its docs for the whole design).
///
/// The nested pump owns no timer — the engine arms none for a `wait_timeout` — so the
/// deadline is recovered from the machine and waited for here. The wait must neither
/// busy-spin (a bare retry loop would burn a core for the whole delay) nor sleep past the
/// deadline (the timeout must fire **no earlier** than `T`, and a message a runnable peer
/// delivers must still win promptly), so every round sleeps `min(remaining, this)` and then
/// pumps the scheduler again.
///
/// It is deliberately a **local** constant rather than the scheduler pass's own sleep bound:
/// that one (`scheduler.rs`'s private `TIMER_PUMP_SLICE`) caps how long a *pass* may sleep on
/// the runtime's own timer heap, while this one caps a sleep **between** two passes of a
/// nested run whose timer that heap never learns about.
const DEADLINE_PUMP_SLEEP: Duration = Duration::from_millis(5);

/// Drives a machine suspended in a `receive` to its `return`, one scheduler pump per wait.
///
/// `step` performs exactly one pump-and-resume for the `(func, pc)` a [`RunOutcome::Wait`]
/// reports — it advances the scheduler by a bounded number of slices and resumes the machine
/// — or returns `Ok(None)` when the mailbox cannot have changed (this context may not
/// re-enter the scheduler, or the pump reported nothing ran). The loop ends when the machine
/// returns, when `step` reports `Ok(None)` (the honest [`beam_engine::blocked`] gap), or when
/// `limit` elapses — the typed deadline diagnostic, the safety net for a process that stays
/// runnable forever. `limit` is a parameter rather than [`BLOCKING_PUMP_DEADLINE`] directly
/// so the deadline path is unit-testable with a short bound.
pub(super) fn pump_until_returned<F>(limit: Duration, mut outcome: RunOutcome, mut step: F) -> Result<Eterm>
where
    F: FnMut(u32, u32) -> Result<Option<RunOutcome>>,
{
    let deadline = Instant::now() + limit;
    loop {
        match outcome {
            RunOutcome::Returned(value) => return Ok(value),
            RunOutcome::Wait { func, pc } => {
                if Instant::now() >= deadline {
                    return Err(error::message(format!(
                        "blocking pump: the scheduler was still runnable after {} ms",
                        limit.as_millis()
                    )));
                }
                match step(func, pc)? {
                    None => {
                        if crate::debug_trace::enabled() {
                            crate::debug_trace::trace(format_args!(
                                "pump_until_returned: nested run parked at {func}/{pc} with a pump that cannot advance -> blocked()"
                            ));
                        }
                        return Err(beam_engine::blocked());
                    }
                    Some(next) => outcome = next,
                }
            }
        }
    }
}

impl<'a> RuntimeContext<'a> {
    /// Runs `module`'s exported `function/arity` with `args`, on this context.
    ///
    /// This is the direct entry point (no BIF/registry indirection): it is what
    /// [`Runtime::run_function`]/[`Runtime::run_module`] use for the top-level run and
    /// what [`ExecutionContext::call_bif`] uses for an external module call.
    ///
    /// It is **blocking-aware**: a callee that suspends in a `receive` is driven by the
    /// scheduler ([`Self::run_export_blocking`]), so a cross-module call that waits on a
    /// runnable process returns rather than reporting a gap.
    ///
    /// # Errors
    ///
    /// [`crate::error::undef`] (class `error`, reason atom `undef`) when the function is
    /// not in the module's atom table or is not exported — exactly the raise the oracle
    /// produces for a call to a function the release does not have — plus any error the
    /// running code raises, and the typed [`beam_engine::blocked`] gap when the callee
    /// suspends with nothing left that could wake it (see [`Self::run_export_blocking`]).
    ///
    /// A raise that escapes the module is passed through [`error::name_atom_raise`], so an
    /// uncaught reason the engine can only carry as a *term* — the bare `function_clause`
    /// of a failed function head, say — is still reported under its own atom name while
    /// its live term is preserved (see that function).
    pub(crate) fn run_export(
        &mut self,
        module: &Module,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let function_atom = module
            .atom_table
            .lookup(function)
            .ok_or_else(error::undef)?;
        if module.export(function_atom, arity).is_none() {
            crate::debug_trace::trace(format_args!(
                "UNDEF no-export {}:{function}/{arity}",
                module.atom_name(module.name).unwrap_or("<unnamed>")
            ));
            return Err(error::undef());
        }
        if crate::debug_trace::enabled() {
            crate::debug_trace::trace(format_args!(
                "sync-run {}:{function}/{arity} (may_pump={})",
                module.atom_name(module.name).unwrap_or("<unnamed>"),
                self.may_pump_scheduler()
            ));
        }
        // Everything this call executes is this module's code, so a BIF reached from it (in
        // particular `erlang:load_nif/2`, whose owning module the oracle derives from the call
        // instruction) must see *this* name. The previous value is restored afterwards, so a
        // nested/external call that returns to the outer module keeps the right identity.
        let previous = self
            .current_module
            .replace(module.atom_name(module.name).unwrap_or_default().to_string());
        let result = self
            .run_export_blocking(module, function_atom, arity, args)
            .map_err(|err| error::name_atom_raise(err, self.runtime.atoms()));
        self.current_module = previous;
        result
    }

    /// Runs `module`'s exported `function/arity` to its `return`, **driving the scheduler**
    /// while the callee is suspended in a `receive`.
    ///
    /// This is [`ExecutionContext::call_bif`]'s external-module route made blocking-aware: a
    /// cross-module callee whose `receive` finds no message does not collapse into the typed
    /// [`beam_engine::blocked`] gap when the process it waits on is merely *runnable*. It is
    /// what makes `proc_lib:start_link/3` — whose `sync_start/2` bare `receive` waits for its
    /// spawned child's `{'DOWN', …}` — return through the `-eval` evaluator, exactly the
    /// blocking-aware behaviour [`crate::boot::drive`] gives a boot-script `{apply, …}` and
    /// [`Runtime::run_scheduled`](crate::runtime::Runtime::run_scheduled) gives a `run`
    /// workload.
    ///
    /// The `Machine` is **reused** across a suspension ([`Machine::resume_outcome`] does not
    /// `reset`), so the callee's registers, Erlang stack and call frames survive the pump and
    /// it continues at exactly the reported `(func, pc)`.
    ///
    /// # When it cannot pump
    ///
    /// A context that is itself inside a scheduler slice ([`RuntimeContext::for_process`],
    /// `may_pump_scheduler() == false`) must never re-enter the scheduler — that would run a
    /// second process from within a slice — so it keeps the plain non-blocking entry's typed
    /// [`beam_engine::blocked`] gap. A pump that reports **nothing ran** (`0`) is the same
    /// honest gap — no process could have changed the mailbox — **unless** the parked
    /// `receive` has a statically readable finite `after T`: that deadline is the nested run's
    /// own to wait out ([`RuntimeContext::pump_once`]), so an empty run queue is then the
    /// expected state and the bound is the delay itself, not `0`. A process that stays
    /// **runnable forever**, by contrast, reports progress on every round;
    /// [`pump_until_returned`] then gives up when its [`BLOCKING_PUMP_DEADLINE`] wall-clock
    /// cap elapses and reports a typed [`error::message`] diagnostic rather than spinning
    /// without end.
    ///
    /// # Why the paused terms stay valid
    ///
    /// A collection moves terms in place, so it must never run while a machine whose roots it
    /// cannot see is suspended. This crate collects **only** at the slice's own heap-admission
    /// point, and only when its per-call GC latch is armed
    /// ([`RuntimeContext::arm_gc`] — the sole armer is the slice's `BudgetContext`, see
    /// [`crate::context::gc`]). This context is never a slice, so its latch is never armed, the
    /// pump's allocations fall back to the pointer-stable arena and nothing pauses here is
    /// moved — the suspended machine's terms stay valid across the resume. Arming the latch
    /// here too would move the paused roots.
    pub(super) fn run_export_blocking(
        &mut self,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        // The fresh `Machine` below holds its `x`/stack outside the process root set a
        // delivery-driven collection publishes, so the process is relocation-exempt for the
        // whole run (see [`Self::with_relocation_exempt`]).
        self.with_relocation_exempt(|this| {
            let mut machine = Machine::new();
            let result = match machine.run_export_outcome(module, function, arity, args, this) {
                Ok(outcome) => pump_until_returned(BLOCKING_PUMP_DEADLINE, outcome, |func, pc| {
                    this.pump_once(&mut machine, module, func, pc)
                }),
                Err(err) => Err(err),
            };
            // The machine is dropped when this scope ends, releasing every off-heap payload
            // handle it built — but the returned term (and any term the callee stored where
            // this run cannot see it) may still point at one. Adopt them into the runtime's
            // escaped-payload store **before** that drop, on every path, so no live term can
            // dangle (see `Runtime::adopt_escaped_binaries`).
            this.runtime
                .adopt_escaped_binaries(machine.refc_binaries());
            result
        })
    }

    /// Runs the raw entry `label` of `module` to its `return`, **driving the scheduler**
    /// while the callee is suspended in a `receive`.
    ///
    /// This is [`Machine::run_label`] made blocking-aware: the label-addressed counterpart of
    /// [`RuntimeContext::run_export_blocking`], entering the body directly rather than
    /// through an export. A local (`make_fun3`) closure has no export name — only a label —
    /// so a fun whose body blocks (`gen:do_call/4`'s `receive`, reached through
    /// `gen:do_for_proc/2`'s `call_fun2`) would otherwise collapse into the typed
    /// [`beam_engine::blocked`] gap even though the process it waits on is merely *runnable*.
    ///
    /// The `Machine` is **reused** across a suspension ([`Machine::resume_outcome`] does not
    /// `reset`), so the closure's registers, its captured free variables and its Erlang stack
    /// survive the pump and it continues at exactly the reported `(func, pc)`. The
    /// non-blocking case is byte-for-byte [`Machine::run_label`], which is itself
    /// `fold_outcome(run_label_outcome(…))`.
    ///
    /// # When it cannot pump
    ///
    /// A context that is itself inside a scheduler slice ([`RuntimeContext::for_process`],
    /// `may_pump_scheduler() == false`) must never re-enter the scheduler — that would run a
    /// second process from within a slice — so it keeps the plain non-blocking entry's typed
    /// [`beam_engine::blocked`] gap. A pump that reports **nothing ran** (`0`) is the same
    /// honest gap — no process could have changed the mailbox — **unless** the parked
    /// `receive` has a statically readable finite `after T`: that deadline is the nested run's
    /// own to wait out ([`RuntimeContext::pump_once`]), so an empty run queue is then the
    /// expected state and the bound is the delay itself, not `0`. A process that stays
    /// **runnable forever**, by contrast, reports progress on every round;
    /// [`pump_until_returned`] then gives up when its [`BLOCKING_PUMP_DEADLINE`] wall-clock
    /// cap elapses and reports a typed [`error::message`] diagnostic rather than spinning
    /// without end.
    ///
    /// # Why the paused terms stay valid
    ///
    /// A collection moves terms in place, so it must never run while a machine whose roots it
    /// cannot see is suspended. This crate collects **only** at the slice's own heap-admission
    /// point, and only when its per-call GC latch is armed
    /// ([`RuntimeContext::arm_gc`] — the sole armer is the slice's `BudgetContext`, see
    /// [`crate::context::gc`]). This context is never a slice, so its latch is never armed, the
    /// pump's allocations fall back to the pointer-stable arena and nothing paused here is
    /// moved — the suspended machine's terms stay valid across the resume. Arming the latch
    /// here too would move the paused roots.
    pub(super) fn run_label_blocking(
        &mut self,
        module: &Module,
        label: Label,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        // The closure's fresh `Machine` holds its `x`/stack outside the process root set a
        // delivery-driven collection publishes, so the process is relocation-exempt for the
        // whole run (see [`Self::with_relocation_exempt`]).
        self.with_relocation_exempt(|this| {
            let mut machine = Machine::new();
            let result = match machine.run_label_outcome(module, label, arity, args, this) {
                Ok(outcome) => pump_until_returned(BLOCKING_PUMP_DEADLINE, outcome, |func, pc| {
                    this.pump_once(&mut machine, module, func, pc)
                }),
                Err(err) => Err(err),
            };
            // As [`Self::run_export_blocking`]: adopt the closure's off-heap payload handles
            // before the machine holding them is dropped.
            this.runtime
                .adopt_escaped_binaries(machine.refc_binaries());
            result
        })
    }

    /// One pump-and-resume of a blocked cross-module run: advance the scheduler, then resume
    /// `machine` at the address its [`RunOutcome::Wait`] reported.
    ///
    /// `Ok(None)` means the mailbox cannot have changed since the last scan — either this
    /// context may not re-enter the scheduler ([`RuntimeContext::for_process`] is itself
    /// inside a slice) or the pump reported nothing ran **and** the parked `receive` has no
    /// finite deadline to wait for — both the honest [`beam_engine::blocked`] gap (see
    /// [`RuntimeContext::run_export_blocking`]). A pump error (a process-table/port failure)
    /// is propagated unchanged.
    ///
    /// # The finite `after T` a nested run must wait out itself
    ///
    /// The engine arms **no** timer for a `receive … after T`: a non-zero `wait_timeout`
    /// simply suspends at its scan label, and the host recovers the deadline from the machine
    /// through [`Machine::wait_deadline`](beam_engine::Machine::wait_deadline) — the
    /// [`WaitDeadline { delay_ms, resume }`](WaitDeadline) pair, whose `resume` is the
    /// `timeout` opcode directly after the parked `wait_timeout` (the C-faithful timed-out
    /// path: the `timeout` instruction resets the scan and continues).
    ///
    /// A **scheduled** process gets that timer from the scheduler pass (a receive deadline it
    /// arms and resumes), but the machine a nested run drives is not scheduled, so nothing
    /// would ever fire its deadline: with no other process runnable the pump reports `0` on
    /// **every** round, so folding that into `Ok(None)` would turn a finite `after T` that only
    /// its own timeout can satisfy into the typed `blocked()` gap. When the parked
    /// `wait_timeout` carries a statically readable finite delay this method therefore waits it
    /// out:
    ///
    /// * **Each round** advances the runtime by [`RUN_EXPORT_PUMP_SLICES`] slices, through
    ///   [`Runtime::pump_scheduler_boot`](crate::runtime::Runtime::pump_scheduler_boot) — the
    ///   **imminent-only** sleep policy the boot drives use, so a round whose run queue is empty
    ///   returns immediately instead of sleeping a full pump slice on a deadline far out (a
    ///   `gen_server` housekeeping timer, say). A pump that reported progress means the mailbox
    ///   may have changed, so the machine is resumed at the address the [`RunOutcome::Wait`]
    ///   reported: a matching message **wins the race** and its outcome is returned immediately
    ///   (a message arriving before `T` cancels the deadline, exactly as in the C).
    /// * **`ran == 0` does not end this branch.** An empty run queue is the *expected* state
    ///   here — nobody but the timeout can answer the receive — so the bound is the finite
    ///   delay itself, never an unbounded wait and never a busy-spin: each round sleeps
    ///   `min(remaining, DEADLINE_PUMP_SLEEP)` before pumping again.
    /// * **The timeout fires only once the wall clock has really reached `start + delay_ms`**
    ///   (`Instant` is monotonic and the target is never clamped below `T`), so it can never be
    ///   delivered early — a wrong value. The resume then re-enters the machine at the `timeout`
    ///   continuation and that outcome is returned.
    /// * A resume that re-parks the machine at a **different** suspension (it took the message
    ///   and its body blocked again) is handed straight back to the driver loop
    ///   ([`pump_until_returned`]), which re-enters here with the new address so **that**
    ///   receive's own deadline is recovered and waited for in turn. Firing the old `timeout`
    ///   continuation there would continue an abandoned receive.
    ///
    /// The wait is bounded by `delay_ms` alone, so the outer [`BLOCKING_PUMP_DEADLINE`] —
    /// checked by [`pump_until_returned`] *between* rounds — never truncates it: a
    /// `receive … after 60000` really waits its full minute, exactly as the oracle does. The cap
    /// still protects a process that keeps returning to the driver loop without ever reaching a
    /// deadline, which is what it is for.
    pub(super) fn pump_once(
        &mut self,
        machine: &mut Machine,
        module: &Module,
        func: u32,
        pc: u32,
    ) -> Result<Option<RunOutcome>> {
        if !self.may_pump_scheduler() {
            return Ok(None);
        }
        // The parked `receive`'s statically readable finite deadline, when it has one: a bare
        // `wait`, an `after infinity`, an unreadable delay and "not parked at a wait_timeout at
        // all" all mean "no deadline" (the honest answer, never a guessed one).
        let finite = match machine.wait_deadline(module) {
            Some(WaitDeadline {
                delay_ms: Some(delay_ms),
                resume,
            }) => Some((delay_ms, resume)),
            _ => None,
        };
        let Some((delay_ms, resume)) = finite else {
            // No deadline: one pump, and a pump that changed nothing ends the drive with the
            // typed gap (the mailbox cannot have changed).
            let ran = self.runtime.pump_scheduler_boot(RUN_EXPORT_PUMP_SLICES)?;
            if ran == 0 {
                return Ok(None);
            }
            return Ok(Some(machine.resume_outcome(module, func, pc, self)?));
        };
        let target = Instant::now() + Duration::from_millis(delay_ms);
        loop {
            if self.runtime.pump_scheduler_boot(RUN_EXPORT_PUMP_SLICES)? > 0 {
                let outcome = machine.resume_outcome(module, func, pc, self)?;
                match outcome {
                    RunOutcome::Returned(_) => return Ok(Some(outcome)),
                    RunOutcome::Wait { func: f, pc: p } => {
                        // The machine re-parked. If it is no longer waiting on **this** receive
                        // (the message arrived and its body blocked in a new one), hand the new
                        // position back to the driver: it re-enters here and recovers the new
                        // receive's own deadline.
                        if machine.wait_deadline(module)
                            != Some(WaitDeadline {
                                delay_ms: Some(delay_ms),
                                resume,
                            })
                        {
                            return Ok(Some(RunOutcome::Wait { func: f, pc: p }));
                        }
                    }
                }
            }
            let now = Instant::now();
            if now >= target {
                // The deadline has really elapsed: continue at the `timeout` opcode.
                return Ok(Some(
                    machine.resume_outcome(module, resume.0, resume.1, self)?,
                ));
            }
            std::thread::sleep(
                target
                    .saturating_duration_since(now)
                    .min(DEADLINE_PUMP_SLEEP),
            );
        }
    }
}

/// The **discriminator** for [`RuntimeContext::run_label_blocking`]: applying a local
/// (`make_fun3`) closure whose body blocks in a `receive` must **suspend and resume**, not
/// fold into the typed [`beam_engine::blocked`] gap.
///
/// A `call_fun` reaches [`ExecutionContext::apply_fun`], whose `Some(LocalFunTarget)` arm
/// enters the closure's entry label through [`RuntimeContext::run_label_blocking`] — the
/// label-addressed twin of [`RuntimeContext::run_export_blocking`]. When the closure's bare
/// `receive` finds no message, that entry drives the scheduler for a bounded number of
/// slices ([`RUN_EXPORT_PUMP_SLICES`]) and resumes the **same** [`Machine`] at the reported
/// `(func, pc)`. The plain non-blocking [`Machine::run_label`] folds that suspension into
/// `unsupported_opcode: run: process blocked (no message available)` at the `call_fun` —
/// which is exactly what the assertions below rule out.
///
/// Nothing here can hang: the pump is bounded, the waiting message is enqueued **before**
/// the call (a child process, not a wall-clock timer), and the control phase uses a context
/// that may not pump at all.
///
/// # Why the fixtures cross a module boundary
///
/// The Erlang compiler **inlines** a `fun` that is bound and called from the same function
/// (it even does so through a local helper), turning the `call_fun` into a direct local
/// call and bypassing [`ExecutionContext::apply_fun`] entirely. The waiting closure is
/// therefore handed to a function in a **second** module — the shape
/// `gen:do_for_proc/2`'s `call_fun2` really has — so the closure is genuinely materialized
/// with `make_fun3` and genuinely applied with `call_fun`.
#[cfg(test)]
mod blocking_fun_apply {
    use super::super::RuntimeContext;
    use crate::context::test_support::{compile_fixture, find_on_path, resolve_erlc, skip_loudly};
    use crate::runtime::Runtime;
    use beam_process::ProcessTable;
    use beam_term::make_small;

    /// `bfwait:wait/0` materializes a 0-arity closure that blocks in a bare `receive` and
    /// leaves the module as an argument (which is what defeats the compiler's fun
    /// inlining); `bfwait:sender/2` is the child that delivers the message it waits for.
    const WAIT_SRC: &str = r#"
-module(bfwait).
-export([wait/0, sender/2]).

wait() -> bfhelper:call(fun() -> receive X -> X end end).

sender(To, Msg) -> To ! Msg.
"#;

    /// The closure's receiving module: `call/1` is where the `call_fun` instruction lives,
    /// so the apply reaches [`ExecutionContext::apply_fun`] whatever the compiler did to
    /// the caller.
    const HELPER_SRC: &str = r#"
-module(bfhelper).
-export([call/1]).

call(F) -> F().
"#;

    #[test]
    fn a_blocking_local_closure_suspends_and_resumes() {
        // The pump boots the runtime on demand (`scheduler_pass` → `boot_if_needed`), which
        // resolves the system code path (natively from the filesystem, else `$ERTS_STDLIB_DIR`
        // or the system `erl`). The guard below is conservative — it skips when neither
        // `$ERTS_STDLIB_DIR` nor an `erl` on `$PATH` is available — and this test needs the
        // compiler too. A missing toolchain is a loud skip, never a silent pass.
        if std::env::var_os("ERTS_STDLIB_DIR").is_none() && find_on_path("erl").is_none() {
            skip_loudly(
                "blocking local closure",
                "no `erl` on $PATH and no $ERTS_STDLIB_DIR",
            );
            return;
        }
        let Some(erlc) = resolve_erlc() else {
            skip_loudly(
                "blocking local closure",
                "no `erlc` (and none beside $ERTS_ORACLE either)",
            );
            return;
        };

        let wait_dir = compile_fixture(&erlc, "blockfun", "bfwait", WAIT_SRC);
        let helper_dir = compile_fixture(&erlc, "blockfun", "bfhelper", HELPER_SRC);

        let mut rt = Runtime::with_defaults().expect("the default runtime");
        rt.boot().expect("the boot sequence");
        // `local_fun_target` finds a compiled closure through the **loaded** module set
        // (`code_server.modules()`), and the helper is entered through the in-machine
        // `call_ext` seam: both are cached here, so neither needs the code path.
        let module = rt
            .code_server
            .load_path(wait_dir.join("bfwait.beam"))
            .expect("load bfwait.beam");
        rt.code_server
            .load_path(helper_dir.join("bfhelper.beam"))
            .expect("load bfhelper.beam");

        // The message the closure waits for: a child that sends to this context's pid. It is
        // enqueued **before** the call, so the pump's first round runs it — the resume is
        // driven by real scheduler work, not by a wall-clock delay.
        let payload = make_small(42);
        let me = rt.self_pid();
        rt.spawn_process("bfwait", "sender", vec![me.to_eterm(), payload])
            .expect("spawn the sender child");

        let result = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.run_export(&module, "wait", 0, &[])
        };
        assert_eq!(
            result.expect("`wait/0` suspends inside its closure and is resumed by the sender"),
            payload,
            "the local closure's `receive` must return the message the child sent"
        );
        let mailbox = rt
            .processes
            .get(me)
            .expect("the runtime's own process")
            .mailbox();
        assert!(
            mailbox.is_empty(),
            "the resumed receive removed the matched message"
        );

        // The control: a context that is itself a scheduler slice may never re-enter the
        // scheduler, so the same closure keeps the engine's honest typed gap — no pump, no
        // hang and no fabricated value.
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));
        let pid = table
            .spawn_default()
            .expect("spawn a process in the pass table");
        let result = {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
            ctx.run_export(&module, "wait", 0, &[])
        };
        let error = result.expect_err("a slice context may not re-enter the scheduler");
        assert_eq!(
            error.to_string(),
            beam_engine::blocked().to_string(),
            "a context that may not pump reports the typed `blocked` gap, never a value"
        );
    }
}

/// Pins the nested pump's **bound and sleep policy** through their observable consequence: real
/// stdlib modules must compile in-process on the pumping `-eval` route, byte-identically to the
/// oracle — including the modules that weigh the most on this route, which is what
/// [`BLOCKING_PUMP_DEADLINE`] must clear.
///
/// # What this pins
///
/// `compile:file/2` is driven through [`Runtime::eval`] — the `-eval` evaluator's
/// `Mod:Func(Args)` route, whose module call reaches
/// [`ExecutionContext::call_bif`](beam_engine::ExecutionContext::call_bif) and so
/// [`RuntimeContext::run_export`] → [`RuntimeContext::run_export_blocking`] →
/// [`pump_until_returned`] → [`RuntimeContext::pump_once`]. Each pump round is a
/// [`Runtime::pump_scheduler_boot`](crate::runtime::Runtime::pump_scheduler_boot) pass, whose
/// **imminent-only** sleep policy leaves a round unslept when its only pending deadline is far
/// out — and a compile does leave one pending (a `gen_server`/`time` timer an hour away). A
/// round that slept in that situation instead (the `TimerWait::AtMost` policy, a full pump slice
/// of 100 ms, roughly once per compiler top-level form) would leave the pump too little of
/// [`BLOCKING_PUMP_DEADLINE`] to finish a large module, so `lists.erl` overran an earlier,
/// smaller cap and produced no artifact at all.
///
/// # Cost, and why these modules
///
/// The compile is purely CPU-bound under the debug runtime, and the set is chosen for the
/// **shape** this cap must clear rather than for coverage: `ordsets` is the small ordinary case,
/// `lists` the next step up, and the remaining five are the largest compiles on this route —
/// per-module `user` time on this host (load-dependent, so the print-out below is the live
/// figure): `dets` ≈ 100 s, `erl_eval` ≈ 83 s, `erl_lint` ≈ 107 s, `shell` ≈ 81 s, `qlc` ≈ 155 s.
/// The whole set therefore costs ≈ 9 minutes of wall clock under the debug runtime on this host,
/// which is why it is one test and not seven. A regression
/// either in [`RuntimeContext::pump_once`]'s sleep policy or in [`BLOCKING_PUMP_DEADLINE`]'s
/// sizing surfaces as the typed `blocking pump` diagnostic or a missing/garbled artifact for one
/// of them, never as a silent pass: every artifact is byte-compared with the oracle's
/// identical-option run.
#[cfg(test)]
mod in_process_compile {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Instant;

    use crate::config::RuntimeConfig;
    use crate::context::test_support::{resolve_oracle, scratch_dir, skip_loudly};
    use crate::runtime::Runtime;

    /// The modules compiled, smallest first: `ordsets` is the ordinary case, `lists` the next
    /// step up, and the remaining five the largest compiles observed on this route (per-module
    /// `user` time in the module docs). Everything the pump's bound has to clear is in here.
    const MODULES: [&str; 7] = [
        "ordsets",
        "lists",
        "dets",
        "erl_eval",
        "erl_lint",
        "qlc",
        "shell",
    ];
    /// The **Erlang string literal** for `path`: the temporary names here are ASCII, which Rust's
    /// `Debug` and Erlang's string syntax quote identically.
    fn literal(path: &Path) -> String {
        format!("{:?}", path.to_str().expect("an ASCII temporary path"))
    }

    /// The repository root: `$ERL_TOP` when set, else three levels up from this crate's manifest
    /// directory (`<root>/erts_rust/crates/beam_rt`).
    fn source_root() -> PathBuf {
        match std::env::var_os("ERL_TOP") {
            Some(value) => PathBuf::from(value),
            None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
        }
    }

    /// The `compile:file/2` expression both runtimes are asked for, with the option list the
    /// byte comparison needs: `no_docs` keeps the compressed `Docs` chunk out of the artifact, so
    /// the bytes are directly comparable (`{outdir, D}` is not recorded in `Dbgi`, and the
    /// compiler does not create `D` — the caller does).
    fn compile_expr(src: &Path, outdir: &Path) -> String {
        format!(
            "compile:file({}, [no_spawn_compiler_process, deterministic, no_docs, \
             {{outdir, {}}}])",
            literal(src),
            literal(outdir)
        )
    }

    /// A booted runtime on the **CLI's own route**: the plain boot sequence, then the boot
    /// script the `-eval` action drives ([`CodeServer::own_bootstrap_boot_script`] — this
    /// runtime's own release's `bootstrap/bin/start.boot`) before its own instruction.
    ///
    /// The script matters for one concrete reason. Its `kernel_sup` wins the `file_server_2`
    /// name, so the compiler's `file:open/2` round trips are answered by the **real** Erlang
    /// server, whose devices implement the `{read_handle_info, Opts}` io request `epp` sends
    /// while reading a `-moduledoc({file, …})` (`lib/stdlib/src/epp.erl:1437`). With no boot
    /// script the runtime's own native service owns that name instead, and its device arm has no
    /// `read_handle_info` — a typed `unsupported` gap in `src/native/file_server.rs`, on a route
    /// this test and the CLI do not measure. Driving the script here therefore keeps the test on
    /// the very route its name (and the A.9 blocker's command line) refers to.
    ///
    /// The config names a script (`with_resolved_boot_script`) for the same reason
    /// [`crate::cli`] records one before it builds the runtime:
    /// [`RuntimeConfig::drives_a_boot_script`] is the **one** predicate behind the native
    /// `file_server_2` name decision ([`Runtime::new`]), so a runtime built from a config that
    /// names no script claims that name itself and the booted `kernel_sup`'s own
    /// `{file_server, start_link, []}` child then aborts on `{already_started, Pid}` — and
    /// `kernel` is `permanent`, so the whole drive tears down. Naming the script here spawns the
    /// native service **unclaimed**, exactly as on the CLI's route.
    fn cli_route_runtime(root: &Path) -> Runtime {
        let config = RuntimeConfig::default().with_resolved_boot_script(Some("start".to_string()));
        let mut runtime = Runtime::new(config).expect("the default runtime");
        runtime.boot().expect("the boot sequence");
        let script = crate::CodeServer::own_bootstrap_boot_script().unwrap_or_else(|| {
            panic!(
                "this runtime's own release must carry a `bootstrap/bin/start.boot` beside the \
                 source tree at {}",
                root.display()
            )
        });
        runtime
            .boot_from_script(&script)
            .expect("the real boot script runs");
        runtime
    }

    /// Compiles [`MODULES`] in-process and byte-compares each artifact with the oracle's
    /// identical-option run. A missing `erl` or a missing stdlib source is a **loud skip**
    /// (`skip_loudly`), never a silent pass or a panic.
    #[test]
    fn a_real_stdlib_module_compiles_in_process_like_the_oracle() {
        const BANNER: &str = "in-process stdlib compile";
        let Some(oracle) = resolve_oracle() else {
            skip_loudly(BANNER, "no `erl` on $PATH (and no executable $ERTS_ORACLE)");
            return;
        };
        let root = source_root();
        let sources: Vec<(&str, PathBuf)> = MODULES
            .iter()
            .map(|module| {
                (
                    *module,
                    root.join("lib/stdlib/src").join(format!("{module}.erl")),
                )
            })
            .collect();
        // An installed-only tree (or a relocated build) has no `lib/stdlib/src`: that is a
        // missing prerequisite, not a failure of this runtime.
        for (module, src) in &sources {
            if !src.is_file() {
                let why = format!(
                    "no real stdlib source to compile at {} for `{module}`",
                    src.display()
                );
                skip_loudly(BANNER, &why);
                return;
            }
        }

        // One runtime **per module**, on the CLI's own route (see [`cli_route_runtime`]): the
        // compile's `epp`/`save_binary` round trips then address the booted `kernel_sup`'s real
        // `file_server_2`, exactly as they do for `beam_rt -noshell -eval '<compile:file(…)>'`.
        // The per-module runtime is load-bearing rather than tidy: the `-eval` route allocates on
        // an arena this runtime never resets, so driving all seven through **one** runtime retains
        // every earlier compile's garbage and the largest of them dies on that arena's own
        // `allocation failure` rather than on anything the pump's bound decides.
        for (module, src) in &sources {
            let mut runtime = cli_route_runtime(&root);
            let rust_out = scratch_dir(&format!("{module}_rust_out"));
            let oracle_out = scratch_dir(&format!("{module}_oracle_out"));

            // ---- the runtime's artifact ----
            let expr = compile_expr(src, rust_out.path());
            let started = Instant::now();
            let result = runtime.eval(&expr).unwrap_or_else(|error| {
                panic!("the in-process compile of `{module}` must complete: {error}")
            });
            let rendered = runtime.format_term_w(result, None).unwrap_or_else(|error| {
                panic!("the `{module}` compile result must be printable: {error}")
            });
            assert_eq!(
                rendered.trim(),
                format!("{{ok,{module}}}"),
                "the in-process compile of `{module}` must answer the oracle's `{{ok,{module}}}`"
            );
            let rust_artifact = rust_out.join(&format!("{module}.beam"));
            let mine = fs::read(&rust_artifact).unwrap_or_else(|error| {
                panic!(
                    "the in-process compile of `{module}` reported {} but wrote no {}: {error}",
                    rendered.trim(),
                    rust_artifact.display()
                )
            });

            // ---- the oracle's artifact, on the identical option list ----
            let oracle_expr = format!("{}, halt(0).", compile_expr(src, oracle_out.path()));
            let output = Command::new(&oracle)
                .arg("-noshell")
                .arg("-eval")
                .arg(&oracle_expr)
                .arg("-s")
                .arg("init")
                .arg("stop")
                .stdin(Stdio::null())
                .output()
                .unwrap_or_else(|error| {
                    panic!("failed to spawn the oracle '{}': {error}", oracle.display())
                });
            assert!(
                output.status.success(),
                "the oracle '{}' must compile `{module}`: {:?}\n--- stderr ---\n{}",
                oracle.display(),
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
            let oracle_artifact = oracle_out.join(&format!("{module}.beam"));
            let reference = fs::read(&oracle_artifact).unwrap_or_else(|error| {
                panic!("the oracle wrote no {}: {error}", oracle_artifact.display())
            });

            // ---- byte-for-byte ----
            let at = mine
                .iter()
                .zip(reference.iter())
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| mine.len().min(reference.len()));
            assert_eq!(
                mine,
                reference,
                "the in-process `{module}` artifact must be byte-identical to the oracle's \
                 (identical option list); first difference at byte {at} of {} vs {} bytes",
                mine.len(),
                reference.len()
            );
            eprintln!(
                "{module}.erl: in-process compile in {:.3} s, {} bytes, byte-identical to the \
                 oracle",
                started.elapsed().as_secs_f64(),
                mine.len()
            );
        }
    }

    /// Every `lib/stdlib/src/*.erl`, `(module, path)`, sorted by module — discovered from the
    /// **filesystem** alone. `None` when the directory is missing or holds no source, which the
    /// caller reports as a loud skip.
    fn stdlib_sources(src_dir: &Path) -> Option<Vec<(String, PathBuf)>> {
        let entries = fs::read_dir(src_dir).ok()?;
        let mut sources: Vec<(String, PathBuf)> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(std::ffi::OsStr::to_str) == Some("erl"))
            .filter_map(|path| {
                let module = path.file_stem()?.to_str()?.to_string();
                Some((module, path))
            })
            .collect();
        sources.sort();
        (!sources.is_empty()).then_some(sources)
    }

    /// The oracle's verdict for one source, on the identical option list: did it exit 0 and
    /// write the artifact? The stderr is returned so a failure is diagnosable, never a bare
    /// `false`.
    fn oracle_compile(oracle: &Path, src: &Path, module: &str, outdir: &Path) -> (bool, String) {
        let expr = format!("{}, halt(0).", compile_expr(src, outdir));
        let output = Command::new(oracle)
            .arg("-noshell")
            .arg("-eval")
            .arg(&expr)
            .arg("-s")
            .arg("init")
            .arg("stop")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| {
                panic!("failed to spawn the oracle '{}': {error}", oracle.display())
            });
        let artifact = outdir.join(format!("{module}.beam"));
        (
            output.status.success() && artifact.is_file(),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        )
    }

    /// The optional shard selection `BEAM_RT_SWEEP_SHARD=i/n`: keep only the sources whose 0-based
    /// index in the sorted list is `i` modulo `n`.
    ///
    /// The corpus is one long sequential run, so the sweep can be measured in `n` independent
    /// foreground shards whose printed summaries are aggregated. Each shard computes the oracle's
    /// compilable set for its own sources, so a shard is self-consistent on its own and the shards
    /// partition the corpus. A malformed value is a loud harness failure, never a silently
    /// unfiltered run.
    fn shard_selection() -> Option<(usize, usize)> {
        fn malformed(value: &str) -> ! {
            panic!("BEAM_RT_SWEEP_SHARD must be `i/n` with 0 <= i < n, got `{value}`");
        }
        let value = std::env::var("BEAM_RT_SWEEP_SHARD").ok()?;
        let (index, count) = value.split_once('/').unwrap_or_else(|| malformed(&value));
        let index: usize = index.trim().parse().unwrap_or_else(|_| malformed(&value));
        let count: usize = count.trim().parse().unwrap_or_else(|_| malformed(&value));
        if count == 0 || index >= count {
            malformed(&value);
        }
        Some((index, count))
    }

    /// **The full stdlib sweep** (A.9): every `lib/stdlib/src/*.erl` compiled **in-process** on
    /// the pumping `-eval` route, one fresh runtime per source, against the oracle's own
    /// compilable set for the identical option list.
    ///
    /// Setting `BEAM_RT_SWEEP_SHARD=i/n` restricts the run to the sources whose 0-based index in
    /// the sorted list is `i` modulo `n` (see [`shard_selection`]), so the corpus can be measured as
    /// `n` parallel foreground shards whose summaries are aggregated afterwards; without the
    /// variable the whole corpus runs in one go.
    ///
    /// This is the corpus-scale measurement, not an iteration gate: it compiles every source
    /// (≈ 96 of them; tens of minutes of wall clock under the debug runtime on this host —
    /// [`MODULES`]' five large ones dominate) and spawns the oracle once per source. Run it
    /// explicitly with its name as the filter:
    ///
    /// ```text
    /// erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --lib \
    ///     -- --ignored --nocapture every_stdlib_source_compiles_in_process_like_the_oracle
    /// ```
    ///
    /// # What it asserts
    ///
    /// * **set**: every source the oracle compiles, this runtime compiles too — a count alone can
    ///   match while the sets differ, so the difference is asserted, not just the size;
    /// * **count**: the number of in-process successes equals the oracle's compilable count;
    /// * **bytes**: every `.beam` both sides produce is byte-identical.
    ///
    /// The oracle's set is the baseline rather than "every file": with this option list (no
    /// `-I`) a source whose includes are not resolvable from its own directory is not compilable,
    /// and that is the oracle's own limit, not this runtime's. Every residual is printed with the
    /// reason this runtime gave, so the count is never a bare number.
    ///
    /// Each source gets a **fresh** runtime on the CLI's own route ([`cli_route_runtime`]): the
    /// `-eval` route allocates on a never-reset arena, so one runtime for ~96 compiles would
    /// retain every compile's garbage at once. A per-source runtime bounds that, keeps one
    /// source's exhaustion from deciding the rest, and costs one boot plus one boot-script drive
    /// per source. A missing `erl`, or a tree without `lib/stdlib/src`, is a **loud skip** — both
    /// are discovered from the filesystem, and the toolchain is never run to be discovered.
    #[test]
    #[ignore = "corpus-scale A.9 sweep: compiles every lib/stdlib/src source (tens of minutes)"]
    fn every_stdlib_source_compiles_in_process_like_the_oracle() {
        const BANNER: &str = "in-process stdlib sweep";
        let Some(oracle) = resolve_oracle() else {
            skip_loudly(BANNER, "no `erl` on $PATH (and no executable $ERTS_ORACLE)");
            return;
        };
        let root = source_root();
        let src_dir = root.join("lib/stdlib/src");
        let Some(sources) = stdlib_sources(&src_dir) else {
            skip_loudly(
                BANNER,
                &format!("no real stdlib source tree at {}", src_dir.display()),
            );
            return;
        };
        let corpus_len = sources.len();
        let shard = shard_selection();
        let sources: Vec<(String, PathBuf)> = match shard {
            Some((index, count)) => sources
                .into_iter()
                .enumerate()
                .filter(|(position, _)| position % count == index)
                .map(|(_, source)| source)
                .collect(),
            None => sources,
        };

        let started = Instant::now();
        let mut oracle_ok: Vec<String> = Vec::new();
        let mut rust_ok: Vec<String> = Vec::new();
        let mut mismatched: Vec<String> = Vec::new();
        let mut oracle_only: Vec<String> = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        for (module, src) in &sources {
            // ---- the oracle's own verdict: the baseline ----
            let oracle_out = scratch_dir(&format!("sweep_oracle_{module}"));
            let oracle_artifact = oracle_out.join(&format!("{module}.beam"));
            let (compiled, stderr) = oracle_compile(&oracle, src, module, oracle_out.path());
            if !compiled {
                failures.push(format!(
                    "{module}: the ORACLE could not compile it with this option list \
                     (stderr: {stderr})"
                ));
                eprintln!("[{}/{}] {module}: oracle FAILED", rust_ok.len(), sources.len());
                continue;
            }
            oracle_ok.push(module.clone());
            let reference = fs::read(&oracle_artifact).unwrap_or_else(|error| {
                panic!("the oracle reported success for `{module}` but wrote no artifact: {error}")
            });

            // ---- the runtime's verdict, in-process, on a runtime of its own ----
            let rust_out = scratch_dir(&format!("sweep_rust_{module}"));
            let rust_artifact = rust_out.join(&format!("{module}.beam"));
            let expr = compile_expr(src, rust_out.path());
            let mut runtime = cli_route_runtime(&root);
            let mine = match runtime.eval(&expr) {
                Ok(_) if rust_artifact.is_file() => {
                    Ok(fs::read(&rust_artifact).unwrap_or_else(|error| {
                        panic!("`{module}` compiled but its artifact is unreadable: {error}")
                    }))
                }
                Ok(_) => Err(format!(
                    "reported success but wrote no {}",
                    rust_artifact.display()
                )),
                Err(error) => Err(error.to_string()),
            };
            drop(runtime);

            let mine = match mine {
                Ok(bytes) => bytes,
                Err(reason) => {
                    oracle_only.push(module.clone());
                    failures.push(format!("{module}: {reason}"));
                    eprintln!(
                        "[{}/{}] {module}: in-process FAILED: {reason}",
                        rust_ok.len(),
                        sources.len()
                    );
                    continue;
                }
            };
            rust_ok.push(module.clone());
            if mine != reference {
                let at = mine
                    .iter()
                    .zip(reference.iter())
                    .position(|(a, b)| a != b)
                    .unwrap_or_else(|| mine.len().min(reference.len()));
                mismatched.push(format!(
                    "{module}: first byte difference at {at} of {} vs {} bytes",
                    mine.len(),
                    reference.len()
                ));
            }
            eprintln!(
                "[{}/{}] {module}: {:.3} s, {} bytes, {}",
                rust_ok.len(),
                sources.len(),
                started.elapsed().as_secs_f64(),
                mine.len(),
                if mine == reference {
                    "byte-identical".to_string()
                } else {
                    "BYTE MISMATCH".to_string()
                }
            );
        }

        eprintln!(
            "stdlib sweep: {} sources in {:.1} s; oracle compiled {}, in-process compiled {}, \
             oracle-only {}, byte-mismatched {}{}",
            sources.len(),
            started.elapsed().as_secs_f64(),
            oracle_ok.len(),
            rust_ok.len(),
            oracle_only.len(),
            mismatched.len(),
            match shard {
                Some((index, count)) => format!(
                    " [shard {index}/{count} of {corpus_len} sources; oracle-compiled and \
                     in-process-compiled sets above are this shard's]"
                ),
                None => String::new(),
            }
        );
        for failure in &failures {
            eprintln!("  residual: {failure}");
        }
        assert!(
            oracle_only.is_empty(),
            "every source the oracle compiles must compile in-process too; {} did not:\n{}",
            oracle_only.len(),
            oracle_only.join(", ")
        );
        assert_eq!(
            rust_ok.len(),
            oracle_ok.len(),
            "the in-process compile count must equal the oracle's own compilable count ({}):\n{}",
            oracle_ok.len(),
            failures.join("\n")
        );
        assert!(
            mismatched.is_empty(),
            "every `.beam` this runtime writes must be byte-identical to the oracle's:\n{}",
            mismatched.join("\n")
        );
    }
}

/// Pins the **bound itself**, in isolation from the compile that sizes it: a nested pump whose
/// awaited run never finishes must still end with the typed diagnostic, never a hang.
///
/// [`pump_until_returned`] takes its `limit` as a parameter for exactly this: production passes
/// the generous [`BLOCKING_PUMP_DEADLINE`], so a test that exercised the production value would
/// have to run for it. The loop below is the documented loop, though — `step` reports progress
/// on **every** round (the shape of a peer that stays runnable forever), which is why the
/// `Ok(None)` exit never fires and why a total cap is the only sound bound (see
/// [`BLOCKING_PUMP_DEADLINE`] for why a forward-progress trip would be unsound).
#[cfg(test)]
mod nested_pump_bound {
    use std::time::{Duration, Instant};

    use beam_engine::RunOutcome;

    /// A `step` that always reports the same parking position, i.e. a pump that never lets the
    /// awaited run finish, ends the drive with the typed diagnostic once `limit` elapses.
    #[test]
    fn a_pump_that_never_lets_its_run_finish_gives_up_with_the_typed_diagnostic() {
        let limit = Duration::from_millis(50);
        let parked = || RunOutcome::Wait { func: 3, pc: 7 };
        let mut rounds = 0u64;
        let started = Instant::now();
        let error = super::pump_until_returned(limit, parked(), |_func, _pc| {
            rounds += 1;
            // A real round costs wall clock (scheduler work plus a resume); a token sleep keeps
            // this rig from spinning hot while still guaranteeing the cap is reached.
            std::thread::sleep(Duration::from_millis(5));
            Ok(Some(parked()))
        })
        .expect_err("a pump that never lets its run finish must trip the cap, never hang");
        assert!(rounds > 0, "the drive pumped at least once before giving up");
        assert!(
            started.elapsed() >= limit,
            "the diagnostic must be reported only after the bound really elapsed"
        );
        assert_eq!(
            error.to_string(),
            crate::error::message(format!(
                "blocking pump: the scheduler was still runnable after {} ms",
                limit.as_millis()
            ))
            .to_string(),
            "the drive reports the typed deadline diagnostic, never a hang and never a value"
        );
    }

    /// The production cap is a **safety net sized to the heaviest legitimate workload on this
    /// route**, so a silent shrink below it is itself a regression. The floor asserted here is
    /// the largest measured `user` time for an in-process stdlib compile plus headroom (the
    /// figures and their provenance are on [`super::BLOCKING_PUMP_DEADLINE`] and the
    /// `in_process_compile` test that drives them).
    #[test]
    fn the_production_cap_clears_the_heaviest_in_process_compile() {
        assert!(
            super::BLOCKING_PUMP_DEADLINE >= Duration::from_secs(300),
            "the blocking-pump cap must clear the largest measured in-process compile (≈ 188 s of \
             wall clock, ≈ 155 s of `user`, on a loaded host) with headroom for a slower one; got \
             {:?}",
            super::BLOCKING_PUMP_DEADLINE
        );
    }
}
