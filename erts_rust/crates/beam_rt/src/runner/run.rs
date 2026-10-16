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
use super::entry::{ProcessEngine, ProcessEntry, RunnerState};
use super::perf::record_baseline_code;
use super::reason::{exit_reason, normal};
use super::tier::select_entry_tier;

// The number of off-heap payloads this thread's slices have registered onto a process's
// off-heap table, for the cursor regression pin (see
// `super::tests::a_process_registers_each_off_heap_payload_exactly_once_across_slices`).
//
// The counter backs a **deterministic** observation of the registration seam: the total number
// of `register_off_heap_binary` calls the runner makes over a process's whole life is
// `== the number of distinct payloads it built` under the reservoir cursor, but
// `>= slices × payloads` under the old whole-reservoir walk. It is **per thread** (not a shared
// `static`) so a concurrently running unit test's slices cannot perturb this test's count — a
// shared atomic would make the pin flaky under `cargo test`'s parallel harness.
#[cfg(test)]
thread_local! {
    pub(crate) static REGISTERED_PAYLOAD_TOTAL: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// The payloads registered on this thread since the last [`reset_registered_payload_total`].
#[cfg(test)]
pub(crate) fn registered_payload_total() -> usize {
    REGISTERED_PAYLOAD_TOTAL.with(std::cell::Cell::get)
}

/// Zeroes this thread's registration counter (the pin's "before" reading).
#[cfg(test)]
pub(crate) fn reset_registered_payload_total() {
    REGISTERED_PAYLOAD_TOTAL.with(|counter| counter.set(0));
}

// The slice/park accounting for this thread's runner, for the **deterministic** counter pin on
// a growth-shaped workload (see `super::tests`).
//
// Like `REGISTERED_PAYLOAD_TOTAL`, these are **per thread** (a `thread_local!`, not a shared
// static) so a concurrently running unit test's slices cannot perturb this test's counts. They
// are **structural** facts of the runner — one *slice* is one `RuntimeRunner::run` call, a
// *park* is a slice that handed the machine back to the scheduler (the `Yield` pre-emption arm
// or the `receive` `Wait` arm) — never timings, so a per-call ratio derived from them is
// deterministic on a loaded host. The `ctx/gc` debt pin's sibling
// (`beam_process::Process::collection_debt`) is the other half: together they say whether the
// per-call work of a workload is **flat** or **N-dependent**.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RunSliceCounters {
    /// `RuntimeRunner::run` calls (one reduction slice each).
    pub(crate) slices: usize,
    /// Slices that ended at the `Yield` (reduction-budget pre-emption) arm.
    pub(crate) yields: usize,
    /// Slices that ended at the `receive` `Wait` arm.
    pub(crate) waits: usize,
}

#[cfg(test)]
thread_local! {
    pub(crate) static RUN_SLICE_COUNTERS: std::cell::Cell<RunSliceCounters> =
        const { std::cell::Cell::new(RunSliceCounters { slices: 0, yields: 0, waits: 0 }) };
}

/// This thread's slice/park counters since the last [`reset_run_slice_counters`].
#[cfg(test)]
pub(crate) fn run_slice_counters() -> RunSliceCounters {
    RUN_SLICE_COUNTERS.with(std::cell::Cell::get)
}

/// Zeroes this thread's slice/park counters (a measurement's "before" reading).
#[cfg(test)]
pub(crate) fn reset_run_slice_counters() {
    RUN_SLICE_COUNTERS.with(|counter| counter.set(RunSliceCounters::default()));
}

/// Records one entered slice (`RuntimeRunner::run`).
#[cfg(test)]
fn note_slice_entered() {
    RUN_SLICE_COUNTERS.with(|counter| {
        let mut value = counter.get();
        value.slices += 1;
        counter.set(value);
    });
}

/// Records one slice that handed the machine back to the scheduler: a `Yield` pre-emption
/// (`preempted == true`) or a `receive` `Wait` park.
#[cfg(test)]
fn note_slice_parked(preempted: bool) {
    RUN_SLICE_COUNTERS.with(|counter| {
        let mut value = counter.get();
        if preempted {
            value.yields += 1;
        } else {
            value.waits += 1;
        }
        counter.set(value);
    });
}

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

/// Resolves a process engine's **current execution site** for the debug trace —
/// `module:function /arity +pc instr=…` — or `None` when no position is resolvable.
///
/// The module is the machine's active module context when the process is inside a
/// cross-module callee; otherwise it is the process's **entry** module, because
/// [`Machine::current_module`](beam_engine::Machine::current_module) returns `None` for a
/// same-module position (the machine *borrows* the entry module rather than owning it). A
/// parked process is reported at the `(func, pc)` it suspended at; the instruction text is
/// truncated exactly as the raise trace truncates it, so a huge debug-formatted op cannot
/// flood the trace.
fn describe_site(engine: &ProcessEngine) -> Option<String> {
    let module = match engine.machine.current_module() {
        Some(module) => module,
        None => Arc::clone(engine.entry.entry_module()?),
    };
    let (func, pc) = engine.machine.current_code().unwrap_or((0, 0));
    let function = module.functions.get(func as usize);
    let fname = function.and_then(|f| module.atom_table.name(f.name).map(str::to_owned));
    let arity = function.map(|f| f.arity);
    let instr = function
        .and_then(|f| f.code.get(pc as usize))
        .map(|i| format!("{i:?}"))
        .unwrap_or_default();
    let instr = instr.chars().take(160).collect::<String>();
    Some(format!(
        "{}:{} /{} +{pc} instr={instr}",
        module.atom_table.name(module.name).unwrap_or("?"),
        fname.unwrap_or_else(|| "?".into()),
        arity.unwrap_or(0)
    ))
}

/// Publishes a **parked** machine's whole live root set into `pid`'s process root set.
///
/// A suspended process's machine lives in [`RunnerState::engines`], but a machine's registers
/// are only a *copy* of the process's root set. A collection driven from outside a slice —
/// [`Process::deliver`](beam_process::Process::deliver)'s exit cascade and the `beam_process`
/// signal builders are **not** gated by
/// [`Runtime::gc_process_enabled`](crate::runtime::Runtime) — relocates the process's heap
/// against its own `regs`, so the parked machine must have published them first; otherwise
/// such a collection would run against a partial root set and leave a stale word in the
/// machine's `x` ([`crate::context::gc`] documents the hand-off). The **Wait** park arm
/// additionally marks the process relocation-exempt for the parked window (cleared when the
/// process is resumed), so a delivery or signal to a *waiting* receiver is declined rather
/// than collected; the publication is what the **`Yield`** (pre-emption) arm — never marked
/// exempt — and the resume's re-sync rely on.
///
/// The machine's **whole** root set is published — every x register, the whole Erlang stack,
/// and the machine's **live leading float-register (FR) prefix**
/// ([`gc::publish_roots_full`](crate::context::gc::publish_roots_full), bounded by
/// `fr_count`). [`RootSet::ALL_LIVE`] is the conservative choice for the x file: the parked
/// machine's live count is not exposed, over-approximating a copying collector's root set is
/// always *safe* (it can only keep a term alive, never free a live one), and the FR file holds
/// `Eterm` term slots at Milestone 1 so a **live** float register is a root like any other.
///
/// # The pending-exception reason is an extra root
///
/// The machine's live **pending-exception reason** ([`Machine::pending_exception`]) is the one
/// term a parked machine holds that the closed [`RootSet`] shape cannot carry: it is a machine
/// *field*, not an `x`/stack/FR slot. It is therefore read out before the root-set borrow and
/// published as the process's [`Registers::extra_roots`](beam_process::Registers::extra_roots)
/// word — the host escape hatch `beam_process`'s `gather_roots`/`scatter_roots` relocate in
/// place — so a collection that does run (a `Yield`-window one) moves it with the heap and the
/// resume re-syncs it back into the machine. With **no** live pending exception the extra-root
/// set is left **empty** (cleared, never a placeholder), so a stale root can never keep an
/// otherwise-dead term alive.
///
/// A `pid` with no process in `table` has nothing to publish into; the machine is left as it is.
///
/// # `fr_count` — the live-FR prefix, never the whole file
///
/// `fr_count` is the number of **leading** FR slots to publish with the stack (the caller
/// passes [`Machine::fr_mark`](beam_engine::Machine::fr_mark)); it is clamped to the file
/// length inside the helper, and `0` publishes none. The FR file is 1024 words and the
/// overwhelming majority of parks leave it entirely `NIL` (the process never touched a float):
/// appending 1024 `NIL` immediates to the process stack roots nothing, yet costs a 1024-word
/// copy on every park and a 1024-word relocation on every collection that follows it. The
/// count is remembered on the engine (`ProcessEngine::fr_published`) and replayed to
/// [`resume_from_process_roots`], so the resume scatters exactly the region this park
/// published.
///
/// # Why bounding the published region by `fr_mark` is sound
///
/// `fr_mark` advances **only** through [`Machine::set_fr`], and after a slice every FR write
/// went through that seam: the interpreter writes an FR operand exclusively through
/// `Operand::FloatReg`'s `Machine::set_fr`, and the **baseline JIT tier never writes the FR
/// file at all** — its emitter refuses an FR destination (such an instruction deopts to the
/// interpreter, which routes through `set_fr`), generated code holds only the x-register base
/// (`JitRuntime::x_regs`), and the native driver's sole register-file action is
/// `Machine::mark_regs_escaped()` (which suppresses the *reset* refill, never advances the
/// marks). So `fr_mark` is an EXACT upper bound of the live FR prefix: every slot at or above
/// it is `NIL`.
///
/// Being an upper bound is all the soundness needs — and it can only err by OVER-pinning: a
/// slot written and then left `NIL` still raises the mark, and publishing a `NIL` word keeps a
/// term alive that would have died anyway. A `NIL` immediate can never be freed into
/// something live, so publishing `0..fr_mark` can never LOSE a live FR term. The
/// raw-slice writers ([`Machine::fr_mut`]/[`Machine::x_mut`]/[`Machine::with_roots`]) do not
/// advance the mark, but the only one reached in this window is this function's own
/// hand-off, whose scatter relocates the FR slots **in place** (indices preserved, `NIL` left
/// `NIL`), so a collection can never create a non-`NIL` slot beyond `fr_mark` either.
///
/// (And any process whose FR *is* live — `fr_mark() > 0` — additionally carries the inter-slice
/// window's relocation exemption, which the post-slice `fr_mark` check in [`RuntimeRunner::run`]
/// sets and the next slice entry clears: the window's own collection sites decline the move,
/// while **this** publication is what makes a collection that does run relocate the FR words
/// instead of leaving them on the abandoned from-space.)
pub(super) fn publish_parked_roots(
    table: &mut ProcessTable,
    pid: ProcessId,
    machine: &mut Machine,
    fr_count: usize,
) {
    // Read the reason **before** the root-set borrow: `with_roots` takes `machine` mutably for
    // the duration of the closure, so `pending_exception()` cannot be reached inside it.
    let reason = machine.pending_exception().map(|(_, reason)| reason);
    let Some(process) = table.get_mut(pid) else {
        return;
    };
    machine.with_roots(RootSet::ALL_LIVE, |roots| {
        gc::publish_roots_full(process, &roots, fr_count)
    });
    // The extra-root set is exactly the live pending-exception reason: cleared first so a machine
    // with none (or one whose exception was handled) never leaves a stale root behind.
    let extra = process.registers_mut().extra_roots_mut();
    extra.clear();
    if let Some(reason) = reason {
        extra.push(reason);
    }
}

/// Re-syncs a machine about to **resume** from the process's authoritative root set.
///
/// The twin of [`publish_parked_roots`]: while the machine was out of the runner — a `Yield`
/// pre-emption, which is never marked relocation-exempt — a collection may have relocated the
/// process's heap and rewritten the process's `regs` in place, leaving the parked machine's own
/// `x`/`stack` pointing into the abandoned space. The process's root set — not the machine's
/// stale copy — is therefore the truth a resume must run from; the context method reconciles
/// them (including reading the relocated pending-exception reason back into the machine and
/// consuming the process's extra roots) and halts the process's collection if their slices no
/// longer match (see [`RuntimeContext::resync_parked_roots`]).
///
/// `fr_count` is the count the matching park published the FR prefix with (the engine's
/// `ProcessEngine::fr_published`); the resume must pass **the same** count so the relocated
/// stack region is split back into `roots.stack`/`roots.fr` exactly as it was appended (a
/// mismatch halts the process's collection rather than writing a mis-sized block back).
pub(super) fn resume_from_process_roots(
    ctx: &mut RuntimeContext<'_>,
    pid: ProcessId,
    machine: &mut Machine,
    fr_count: usize,
) {
    ctx.resync_parked_roots(pid, machine, fr_count);
}

impl ProcessRunner for RuntimeRunner<'_> {
    /// Runs one slice of process `pid`: its first entry, or the resume of the `receive`
    /// that suspended it.
    ///
    /// # Reduction accounting and pre-emption
    ///
    /// `budget` is the slice's **reduction budget** (the C `CONTEXT_REDS`); the runner owns
    /// the reduction count (the scheduler never counts — see the [`ProcessRunner`]
    /// contract). It is charged by [`BudgetProbe`] from the seams the executing machine
    /// reaches: on the **interpreter** path once per interpreted instruction (the engine's
    /// per-instruction `reduce` hook, which is charged *before* the instruction's body runs)
    /// and, on either tier, at the machine's **heap-admission points** — every
    /// `test_heap`/`allocate_heap` the executing machine runs reaches
    /// [`ExecutionContext::ensure_heap`](beam_engine::ExecutionContext::ensure_heap) once,
    /// which is the only charge a natively executed baseline block reaches. The slice
    /// **ends in [`Yield`](beam_process::RunOutcome::Yield)** once the budget is spent, so a
    /// CPU-bound process is pre-empted and the scheduler requeues it instead of letting it
    /// monopolize the run queue.
    ///
    /// The yield is **resumption-exact**: the machine is parked at
    /// [`Machine::current_code`](beam_engine::Machine::current_code) — the very instruction
    /// the charge was taken for. The engine records that address *before* it prices the
    /// instruction and *before* the instruction's effect, so the resume
    /// ([`Machine::resume_outcome_with_tier`](beam_engine::Machine::resume_outcome_with_tier))
    /// runs it **exactly once**, with the registers, the stack and the frames intact, exactly
    /// as a `receive` resume does. No result is fabricated and no work is repeated.
    ///
    /// The budget is only charged from the **slice's own** machine: the wrapper is the
    /// context of that machine alone, and every machine a BIF reaches (an external module
    /// call, a fun apply) is built inside `RuntimeContext` with the plain context, so a
    /// nested run can never consume the budget nor be mistaken for the resumable machine.
    /// A loop the compiler emits **without** a heap test (`spin/1`-style: no allocation, so
    /// no `test_heap`) is therefore pre-empted on the interpreter tier at the budget like
    /// any other interpreted code. The residual limit is the **native** tier: a compiled
    /// baseline block runs natively and does not cross the per-instruction hook, so such a
    /// loop is charged only where the block deopts or reaches a heap-admission point, and a
    /// natively linked tail loop is not pre-empted mid-block. That is reported as such, never
    /// worked around with a fabricated result.
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
        // The deterministic slice counter (see `RUN_SLICE_COUNTERS`): one `run` call is one
        // reduction slice, whatever the machine does inside it.
        #[cfg(test)]
        note_slice_entered();
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
        // *before* the context borrows the runtime, since the context takes `Runtime` mutably
        // below) and then **remembered on the engine** (`ProcessEngine::tier`), so a resumed
        // process — one that pre-empted on the reduction budget, or parked in a `receive` —
        // re-enters on that same tier instead of falling back to the interpreter. The mode alone
        // decides: the baseline tier is sound for any module (the engine's driver handles the
        // in-machine module-context switch — see `super::tier`), so the entry's module is not
        // inspected.
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
                // The FR prefix is scattered back only as far as the park published it
                // (`ProcessEngine::fr_published`), so the relocated stack region is split
                // exactly as the park appended it.
                let fr_count = engine.fr_published;
                resume_from_process_roots(&mut ctx, pid, &mut engine.machine, fr_count);
                // The parked window is over: clear the park arm's relocation exemption so
                // the running slice below can collect this process normally. (A process
                // resumed from a pre-emption was never marked; clearing is a no-op there.)
                ctx.process_table_mut()
                    .set_relocation_exempt(pid, false)?;
            }
            // The slice's reduction budget, charged per interpreted instruction and at the
            // machine's heap-admission points (see the `run` docs).
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
                        } => {
                            // A process entry that names an MFA the **host serves** must be
                            // dispatched to the host, never entered as the module's own code:
                            // `erlang.beam`'s `send/2` (for `spawn(erlang, send, [Pid, Msg])` —
                            // which is what `timer_server`'s `do_apply/2` runs for a
                            // `timer:apply_after/4` MFA) is
                            // `func_info; move undefined->x0; call_ext_only erlang:nif_error/1`,
                            // so entering it raises the bare atom `undefined` and the message is
                            // never delivered. Real ERTS installs a BIF trampoline at such an
                            // export. The cross-module-call seam already classifies exactly this
                            // question — a floor name, a NIF-loaded module, a `beam_bifs`
                            // registry key (where `erlang:send/2` and `erlang:abs/1` live) —
                            // so the entry asks it the same thing a `call_ext*` does (see
                            // `RuntimeContext::resolve_external_call`): `Some((_, label))` is
                            // the route to code, `None` the route to `call_bif`'s dispatch
                            // order (`try_call_nif` → `try_native` → `call_registered`). The
                            // entered module stays the **entry's own** `Arc<Module>` — the one
                            // the code server caches — so the engine's `Arc::ptr_eq`
                            // module-context dedup is unaffected.
                            //
                            // Both atoms are resolved from the module's **own** atom table
                            // (never the runtime's shared namespace), and a missing one is the
                            // honest typed gap rather than a substituted name — the entry module
                            // and its export were resolved from this very table by
                            // `Runtime::resolve_export_entry`.
                            let names = module
                                .atom_table
                                .name(module.name)
                                .zip(module.atom_table.name(*function));
                            match names {
                                Some((module_name, function_name)) => {
                                    match budget_ctx.resolve_external_call(
                                        module_name,
                                        function_name,
                                        *arity,
                                    ) {
                                        // Entered at the export's own label on the tier chosen
                                        // above (`Tier::BaselineJit` under the default mode,
                                        // `Tier::Interpreter` under the `BEAM_RT_TIER=interpreter`
                                        // kill switch). The label entry is the
                                        // equivalence-preserving route to the export — it calls
                                        // `Machine::enter_run`, so the run's
                                        // `initial_call`/`current_function` are recorded and
                                        // `process_info/2`'s execution-position items can be
                                        // answered exactly as they were before this seam was
                                        // consulted. It is the `Arc<Module>` the code server
                                        // caches (the entry's own), so the engine's `Arc::ptr_eq`
                                        // module-context dedup still holds.
                                        Ok(Some((_, label))) => engine
                                            .machine
                                            .run_label_outcome_with_tier(
                                                module,
                                                label,
                                                *arity,
                                                &engine.args,
                                                &mut budget_ctx,
                                                tier,
                                            ),
                                        // The host serves this MFA: dispatch it exactly as a
                                        // `call_ext` would (`call_bif` applies the NIF → the
                                        // runtime-native floor → the `beam_bifs` registry), and
                                        // the returned value is the process's `x(0)`, recording
                                        // like any other return — so `spawn(erlang, abs, [-3])`
                                        // yields `3`. A fun entry with no local label takes the
                                        // same shape below
                                        // (`apply_fun(..).map(EngineRunOutcome::Returned)`). The
                                        // resolution is idempotent, so it cannot disagree with
                                        // the answer the synchronous path would have given.
                                        Ok(None) => budget_ctx
                                            .call_bif(
                                                module_name,
                                                function_name,
                                                *arity,
                                                &engine.args,
                                            )
                                            .map(EngineRunOutcome::Returned),
                                        Err(error) => Err(error),
                                    }
                                }
                                None => Err(crate::error::unsupported(
                                    "run_export: the entry module/function atom has no name",
                                )),
                            }
                        }
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

        // The **float-register (FR) safety valve**, checked while the machine is no longer
        // running: `Machine::fr` holds *terms* at Milestone 1, so a **live** FR slot is a live
        // root exactly like an x register or a stack word. The module-level float guard
        // (`context::gc::module_uses_floats`) already refuses to collect over a module that
        // contains a float instruction; this catches the residual case — a float register left
        // set by *another* module's code (a callee) while this process runs float-free code.
        //
        // The live prefix is read **once** per slice and its count (`fr_count`) is reused for
        // both reactions below: how much of the FR file the park publishes into the process
        // stack (`publish_parked_roots`), and how long this process must stay un-collectable
        // outside a slice. The two are two halves of one fact — a process with a live FR has
        // terms that must be relocated **and** must not be left exposed between slices — so they
        // are derived from one read, never read twice.
        //
        // # Why the reaction is scoped to the inter-slice window, never to the process's life
        //
        // The FR file is written **only while the process runs**, so a live FR prefix can only be
        // carried *across* a slice: the runner reads it here (the machine has just stopped) and
        // the next slice's entry is the last moment anything may act on it. Outside that window
        // the valve must not be armed at all:
        //
        //   * **inside a slice** the engine reaches only the **full** heap-admission door
        //     (`context::gc::ensure_heap_full`, from `test_heap`/`allocate_heap`), which publishes
        //     the machine's **whole** root set — the FR file included — so a live float register is
        //     relocated exactly like an x register and a float-using call site collects exactly
        //     like a float-free one. Declining *that* door — an earlier shape of this valve, which
        //     marked the process permanently un-collectable — would refuse **every** later
        //     collection, so a float-operation loop could never collect and its nursery would grow
        //     with the program's whole allocation history instead.
        //   * **across the inter-slice window** the park published the live FR prefix onto the
        //     process stack (`publish_parked_roots`, with the same `fr_count`) and the matching
        //     resume scatters that region back into the machine's FR slots
        //     (`resume_from_process_roots`), so a collection that does run in the window relocates
        //     every live FR word rather than leaving it on the abandoned from-space (pinned by
        //     `super::tests::the_published_fr_prefix_round_trips_through_a_moving_collection`).
        //
        // The guard is therefore **per slice**: a process whose FR prefix is live when a slice ends
        // is marked **relocation-exempt** for the window that follows, and the **next slice entry**
        // clears the mark again (`RuntimeRunner::run`'s parking branch), so a window that opens
        // with a dead FR prefix is protected by nothing at all. The mark is decline-only and
        // bounded: a delivery into the window takes the ordinary heap-fragment fallback
        // (`Process::deliver`'s raw refusal), and the process's own admission point collects as
        // soon as the resume has cleared the mark — so nothing grows unboundedly.
        //
        // # Why `fr_mark` is the right — and exact — bound
        //
        // [`Machine::fr_mark`] is one past the highest FR slot a [`Machine::set_fr`] has
        // written since the machine's last reset, so it bounds the live FR prefix: every slot
        // at or above it is `NIL`. It is **exact** after a slice because every FR write in this
        // engine goes through `set_fr`: the interpreter writes an FR operand only through
        // `Operand::FloatReg`, and the **baseline JIT tier never writes the FR file** — its
        // emitter refuses an FR destination (such an instruction deopts to the interpreter,
        // which then routes through `set_fr`), generated code holds only the x-register base
        // (`JitRuntime::x_regs`), and the native driver's only register-file action is
        // `Machine::mark_regs_escaped()`, which suppresses the *reset* refill rather than
        // advancing the marks.
        //
        // Bounding by an upper bound is the safe direction even where it is not exact: it can
        // only OVER-protect (a slot written and then left `NIL` still raises the mark, and
        // publishing a `NIL` immediate keeps a term alive that would have died anyway — it can
        // never free one), so publishing `0..fr_mark` can never lose a live FR term. And a
        // collection scatter cannot create a non-`NIL` slot beyond `fr_mark`: the hand-off
        // relocates the FR region IN PLACE (indices preserved, `NIL` left `NIL`).
        //
        // # Why an empty prefix needs neither reaction
        //
        // A process whose FR prefix is empty (`fr_mark() == 0`) carries no live FR term at all, so
        // omitting the FR words from its published root set cannot lose a term and the window may
        // collect normally. Every site that can move such a process's heap is then sound: the
        // full-root in-slice doors publish the whole FR file (all `NIL`, publishing which is a
        // no-op), `Runtime::serve_gc_request` and `Process::deliver`'s growth cascade collect
        // against the process's own root set, and the narrow door (`ensure_heap`) — off the
        // engine's path — is gated by the caller-module float guard.
        let fr_count = engine.machine.fr_mark() as usize;
        if fr_count > 0 {
            // The machine still holds live FR terms and is about to leave the runner for the
            // inter-slice window: the park below publishes them (`publish_parked_roots`) and this
            // mark declines any relocation those window sites would otherwise drive before the
            // resume re-syncs them. The next slice entry clears it (see above), so the guard never
            // outlives the window in which the FR prefix is actually live.
            if let Some(process) = table.get_mut(pid) {
                process.set_relocation_exempt(true);
            }
        }
        // The `+JPperf` map (contract A.8) must describe the **real** native code this runtime
        // can run, never a fabricated range: the module's baseline artifact is that source, so
        // its cached `[start, size)` regions are snapshotted here -- once per first entry, not
        // per slice. Diagnostic only: `record_baseline_code` records, never propagates.
        if let Some(module) = first_entry_export.as_deref() {
            record_baseline_code(self.runtime, module);
        }

        // Register the off-heap refc binaries this machine built onto the **running process's**
        // own off-heap table, so `erlang:process_info(Pid, binary)` has a roster to report.
        //
        // The real *holders* of those payloads are this machine's never-cleared reservoir
        // (`Machine::refc_binaries`, appended by `Machine::alloc_refc_binary` whenever compiled
        // bitstring code materializes a result above the on-heap limit and never emptied by
        // `Machine::reset`), while the roster `process_info` reads back is
        // `beam_process::Process`'s own table — the thing
        // `Process::off_heap_binaries`/`off_heap_binary_count` enumerate — which is empty until
        // something **registers** them, and nothing else in production does. This seam is what
        // makes a running process come to *own* what its engine built.
        //
        // # Why this can never under-report
        //
        // The seam sits on the common path below the slice and above the outcome dispatch, so
        // it runs on **every** slice outcome — a pre-emption (`Yield`), a `receive` park
        // (`Wait`), a `Returned` and an `Err` alike (the last two belong to a process that is
        // about to terminate, so their registration is merely harmless). A process's roster is
        // therefore a superset of every payload its machine had built when any of its slices
        // ended. The one window it does not cover is a `process_info(self(), binary)` issued
        // from **inside** the running slice itself, before that slice reaches this point: the
        // roster such a process reads back about itself can omit a binary the same slice has
        // just built. That is safe rather than a lost term — the `Machine::refc_binaries`
        // reservoir (of which this registration is only a mirror) is what keeps the payload
        // alive, so a registration that has not happened yet can never *dangle* a payload; the
        // worst it can do is omit an entry from a roster the process reads back about itself.
        //
        // # Why the registration advances a cursor instead of re-walking the reservoir
        //
        // The reservoir is **append-only** and never cleared, so a later slice's reservoir is an
        // earlier slice's reservoir plus whatever that slice built. Re-offering the whole
        // reservoir on every slice would re-offer every already-registered payload on every later
        // slice, and each offer costs an O(slots) `owned_binary_slot` scan inside
        // `Process::register_off_heap_binary` before its idempotent drop — Θ(slices × slots) over a
        // long-lived process, i.e. the quadratic-to-cubic blow-up the `foreign` `enc` workload
        // showed. The engine's `ProcessEngine::registered_payloads` records the reservoir prefix
        // already registered, so each slice registers only its **not-yet-registered suffix** and the
        // total work over the process's whole life is linear in the number of payloads it built.
        //
        // The cursor lives on the `ProcessEngine`, which is removed from and re-inserted into
        // [`RunnerState::engines`](super::entry::RunnerState) at every slice boundary (both park
        // arms re-insert the **same** mutated engine), so it survives a pre-emption and a
        // `receive` park: slices 2..N observe the cursor the earlier slices advanced, not the `0`
        // of a fresh engine. The `min` against `all.len()` keeps the cursor in range even if the
        // reservoir were ever rebuilt smaller (it cannot shrink today, since `Machine::reset` does
        // not clear it), so a stale cursor can only skip work, never index out of bounds or
        // under-register a payload the suffix walk already covered.
        //
        // # Why over-reporting is the safe and faithful direction
        //
        // A payload registered at the end of the first slice that built it stays registered: the
        // cursor never un-registers, so the roster `Process::off_heap_binaries` reads back still
        // lists a payload that `Process::sweep_off_heap_binaries_reachable` pruned at a later
        // rooted collection. That is harmless — `register_off_heap_binary` is dedup-by-`data_ptr`
        // and idempotent, so a repeat drops the surplus handle — and the over-report itself
        // **mirrors the oracle**, which likewise lists a dropped binary until a GC collects it.
        // Listing a payload the never-cleared reservoir still owns is therefore both safe and
        // faithful, never a fabricated entry.
        //
        // # Why `Runtime::eval_off_heap` is deliberately NOT registered here
        //
        // That store is the **pid-less** home of the `-eval` root process's escaping payloads,
        // not this machine's work. Attributing it to this process would fabricate a roster entry
        // for a payload the process never built, so it stays out of this seam entirely.
        if let Some(process) = table.get_mut(pid) {
            let all = engine.machine.refc_binaries();
            let start = engine.registered_payloads.min(all.len());
            for payload in &all[start..] {
                process.register_off_heap_binary(payload.clone());
            }
            #[cfg(test)]
            REGISTERED_PAYLOAD_TOTAL.with(|counter| counter.set(counter.get() + (all.len() - start)));
            engine.registered_payloads = all.len();
        }

        // Pre-emption: the budget ran out at a charging point of this slicing machine
        // (an interpreted instruction's per-instruction charge, or a heap-admission
        // point). Park it at that very instruction and hand the process back to the
        // scheduler, which requeues it (`slice_end`'s `Yield` arm) so a second runnable
        // process gets the next slice. Nothing is recorded as a result/exit and the
        // process stays `Runnable` — it has neither returned nor blocked.
        if exhausted {
            let Some(address) = engine.machine.current_code() else {
                // Unreachable: both charging seams price an instruction the machine has
                // already recorded as its position (`Machine::drive` stores it before the
                // per-instruction charge, and a heap-admission point is reached from an
                // instruction that recorded it in `step_instruction`). Reported as a typed
                // error rather than parked at a guessed address — no wrong result, no hang.
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
            // heap against the machine's live terms (see `publish_parked_roots`). The FR prefix
            // is published exactly as far as `fr_mark` measured it live; the same count is
            // stored on the engine so the resume scatters exactly this region back. A **live**
            // prefix also carries the inter-slice window's relocation exemption (the post-slice
            // `fr_mark` check above), so the window's own collection sites decline the move
            // while this publication is what keeps the FR words relocatable.
            engine.fr_published = fr_count;
            publish_parked_roots(table, pid, &mut engine.machine, fr_count);
            // A `garbage_collect` request (see `Runtime::request_gc`) that no heap-admission
            // point served is honoured **here**: the parked machine's whole root set was just
            // published — the live FR prefix included — so the collector relocates exactly what
            // is live. A process whose FR prefix was *live* at this slice's end carries the
            // window's relocation exemption (set by the post-slice `fr_mark` check), so this
            // request is declined and dropped by the exemption valve like any other
            // valve-refused one; the process's own next heap-admission point — after the resume
            // has cleared the mark — serves it.
            self.runtime.serve_gc_request(table, pid);
            self.state.engines.insert(pid, engine);
            // The deterministic park counter (see `RUN_SLICE_COUNTERS`): this slice ended at
            // the `Yield` (pre-emption) arm.
            #[cfg(test)]
            note_slice_parked(true);
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
                if crate::debug_trace::enabled() {
                    crate::debug_trace::trace(format_args!("process {pid:?} returned normally"));
                }
                Ok(RunOutcome::Exit(normal(self.runtime)))
            }
            Ok(EngineRunOutcome::Wait { func, pc }) => {
                if crate::debug_trace::enabled() {
                    let entry = engine
                        .entry
                        .entry_module()
                        .and_then(|m| m.atom_table.name(m.name).map(str::to_owned));
                    crate::debug_trace::trace(format_args!(
                        "process {pid:?} blocked (no message): entry={entry:?} parked=+{pc} (func {func}) at={:?}",
                        describe_site(&engine)
                    ));
                }
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
                    // Keep the parked process **relocation-exempt** for the whole parked
                    // window. While it waits, a delivery or signal can drive a collection of
                    // its heap (`Process::deliver`'s exit cascade is not gated by
                    // `gc_process_enabled`) against the *published* root snapshot.
                    //
                    // That snapshot is not the whole truth, and the two protections are **not**
                    // equivalent. The installed host collection gate
                    // (`crate::context::gc_walk::RuntimeCollectionGate`) declines only when an
                    // **unscanned container is reachable from the receiver's GATHERED roots**
                    // (`seed_process_roots`); this exemption refuses *any* relocation for the
                    // parked window. The gap it covers is a live term the host holds **outside**
                    // that gathered set — the runner's own entry terms (`ProcessEngine::args`,
                    // `ProcessEntry::Fun::fun`, `LocalFunTarget::args`) and any live `Eterm` in
                    // the host's Rust state — which the walk cannot see, plus
                    // `Runtime::serve_gc_request`, which gates its descent on the per-pid latch
                    // (`foreign_reachable`) rather than always descending, so it can miss a
                    // depth-≥2 foreign container the gate's unconditional descent would catch.
                    // Exempting is **decline-only**: such a delivery takes the ordinary
                    // heap-fragment fallback (`Process::deliver`'s raw refusal), and the
                    // process's own next admission point — after it is resumed, where the
                    // exemption is cleared above — collects as usual, so nothing grows
                    // unboundedly. Pinned by
                    // `super::tests::a_parked_process_stays_relocation_exempt_for_its_parked_window`.
                    process.set_relocation_exempt(true);
                }
                // Publish the machine's roots before the engine is parked. The parked process
                // is relocation-exempt (set above), so the parked window itself is not
                // collected over; the publication is what the resume's re-sync relies on (see
                // `publish_parked_roots`). The FR prefix is published exactly as far as
                // `fr_mark` measured it live, and the same count is stored on the engine for the
                // resume to replay.
                engine.fr_published = fr_count;
                publish_parked_roots(table, pid, &mut engine.machine, fr_count);
                // A pending `garbage_collect` request (see `Runtime::request_gc`) is **not**
                // served here: the exemption set above declines it — `serve_gc_request`
                // consults `Process::is_relocation_exempt` before committing to a collection —
                // so the request is consumed and dropped like any other valve-refused one.
                // The admission points and the `Yield` arm are the sites that serve it.
                self.runtime.serve_gc_request(table, pid);
                self.state.engines.insert(pid, engine);
                // The deterministic park counter (see `RUN_SLICE_COUNTERS`): this slice ended
                // at the `receive` `Wait` arm.
                #[cfg(test)]
                note_slice_parked(false);
                Ok(RunOutcome::Wait)
            }
            Err(error) => {
                let reason = exit_reason(self.runtime, &error);
                if crate::debug_trace::enabled() {
                    let entry = engine
                        .entry
                        .entry_module()
                        .and_then(|m| m.atom_table.name(m.name).map(str::to_owned));
                    crate::debug_trace::trace(format_args!(
                        "process {pid:?} raised: {error:?} entry={entry:?} at={:?}",
                        describe_site(&engine)
                    ));
                }
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
