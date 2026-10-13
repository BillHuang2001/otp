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
/// with the honest typed [`beam_engine::blocked`] gap instead of spinning.
const RUN_EXPORT_PUMP_SLICES: u64 = 64;

/// The wall-clock safety cap on a nested blocking pump (see [`pump_until_returned`]).
///
/// [`RuntimeContext::run_export_blocking`] and [`RuntimeContext::run_label_blocking`] drive
/// the scheduler while a cross-module callee is suspended in a `receive` (one bounded round
/// of [`RUN_EXPORT_PUMP_SLICES`] slices per wait). That loop normally ends as soon as a pump
/// reports **nothing ran** — the callee's peer has answered or is itself blocked — but a
/// process that stays **runnable forever** reports progress on every round and would spin the
/// pump without end. This bound is the nested pump's counterpart of the boot drive's
/// `DRIVE_DEADLINE`: checked between pump rounds, it turns an unbounded pump into a typed
/// diagnostic instead of a hang. It is deliberately generous — no legitimate
/// single-pass/`-eval`/boot workload (a `proc_lib` start, a `gen_server` round trip, an
/// io-device read) comes close — so the cap fires only on a genuine infinite pump.
pub(super) const BLOCKING_PUMP_DEADLINE: Duration = Duration::from_secs(10);

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
                    None => return Err(beam_engine::blocked()),
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
            return Err(error::undef());
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
    /// * **Each round** advances the runtime by [`RUN_EXPORT_PUMP_SLICES`] slices. A pump that
    ///   reported progress means the mailbox may have changed, so the machine is resumed at the
    ///   address the [`RunOutcome::Wait`] reported: a matching message **wins the race** and its
    ///   outcome is returned immediately (a message arriving before `T` cancels the deadline,
    ///   exactly as in the C).
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
            let ran = self.runtime.pump_scheduler(RUN_EXPORT_PUMP_SLICES)?;
            if ran == 0 {
                return Ok(None);
            }
            return Ok(Some(machine.resume_outcome(module, func, pc, self)?));
        };
        let target = Instant::now() + Duration::from_millis(delay_ms);
        loop {
            if self.runtime.pump_scheduler(RUN_EXPORT_PUMP_SLICES)? > 0 {
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
