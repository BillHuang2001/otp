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
/// and the FR file **when it is live** ([`gc::publish_roots_full`](crate::context::gc::publish_roots_full),
/// gated by `publish_fr`). [`RootSet::ALL_LIVE`] is the conservative choice for the x file: the
/// parked machine's live count is not exposed, over-approximating a copying collector's root
/// set is always *safe* (it can only keep a term alive, never free a live one), and the FR file
/// holds `Eterm` term slots at Milestone 1 so a **live** float register is a root like any
/// other.
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
/// # `publish_fr` — the dead-FR fast path
///
/// The FR file is 1024 words and the overwhelming majority of parks leave it entirely `NIL`
/// (the process never touched a float); appending 1024 `NIL` immediates to the process stack
/// roots nothing, yet costs a 1024-word copy on every park and a 1024-word relocation on every
/// collection that follows it. The caller therefore passes `publish_fr` — the `fr_is_live` it
/// already computed from the post-slice scan (the same scan that pins a live-FR pid) — so the
/// FR rides the stack only when it is live. The choice is remembered on the engine
/// (`ProcessEngine::fr_published`) and replayed to [`resume_from_process_roots`], so the resume
/// scatters exactly the region this park published.
///
/// This is sound because an all-`NIL` FR can carry no live term: the collector gathers the
/// process's stack as roots, so appending `NIL` words changes nothing about which terms
/// survive. (And any process whose FR *is* live is pinned by
/// [`Runtime::pin_process`](crate::runtime::Runtime) and never collected — see the note at the
/// post-slice scan in [`RuntimeRunner::run`].)
pub(super) fn publish_parked_roots(
    table: &mut ProcessTable,
    pid: ProcessId,
    machine: &mut Machine,
    publish_fr: bool,
) {
    // Read the reason **before** the root-set borrow: `with_roots` takes `machine` mutably for
    // the duration of the closure, so `pending_exception()` cannot be reached inside it.
    let reason = machine.pending_exception().map(|(_, reason)| reason);
    let Some(process) = table.get_mut(pid) else {
        return;
    };
    machine.with_roots(RootSet::ALL_LIVE, |roots| {
        gc::publish_roots_full(process, &roots, publish_fr)
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
/// `publish_fr` is the value the matching park published the FR file with (the engine's
/// `ProcessEngine::fr_published`); the resume must pass **the same** value so the relocated
/// stack region is split back into `roots.stack`/`roots.fr` exactly as it was appended (a
/// mismatch halts the process's collection rather than writing a mis-sized block back).
pub(super) fn resume_from_process_roots(
    ctx: &mut RuntimeContext<'_>,
    pid: ProcessId,
    machine: &mut Machine,
    publish_fr: bool,
) {
    ctx.resync_parked_roots(pid, machine, publish_fr);
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
                // The FR file is scattered back only if the park that suspended this machine
                // published it (`ProcessEngine::fr_published`), so the relocated stack region
                // is split exactly as the park appended it.
                let fr_published = engine.fr_published;
                resume_from_process_roots(&mut ctx, pid, &mut engine.machine, fr_published);
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
                        } => match module.export(*function, *arity) {
                            // Entered at the export's own label on the tier chosen above
                            // (`Tier::BaselineJit` under the default mode, `Tier::Interpreter`
                            // under the `BEAM_RT_TIER=interpreter` kill switch). The baseline
                            // driver handles the in-machine module-context switch itself: a
                            // `call_ext*` the host resolves through
                            // `ExecutionContext::resolve_external_call` (see
                            // `src/context/mod.rs`) is entered in this same machine as a
                            // `Step::Enter` — the `ENTER` block-ABI word, resolved and
                            // re-entered in native code (see
                            // `crates/beam_engine/src/jit/CONTEXT.md`). The tier is therefore
                            // sound for any module and needs no per-module inspection; the
                            // selection and its soundness argument live in `super::tier`. The
                            // resume path above re-enters on the tier this process was selected
                            // for (remembered in `ProcessEngine::tier`), so both halves of a
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
        //
        // The scan runs **once** per slice and its answer (`fr_is_live`) is reused for both
        // reactions below: the pin, and the park's decision to publish the FR file into the
        // process stack (`publish_parked_roots`'s `publish_fr`). The two are two halves of one
        // fact — a process with a live FR is pinned *and* must have that FR relocated — so they
        // are derived from one scan, never scanned twice.
        //
        // # Why the park's FR skip is sound against every collection site
        //
        // A process whose FR is all `NIL` carries no live FR term, so omitting the 1024 FR
        // words from its published root set cannot lose a term. Every site that can move a
        // process heap collects a process for which that fact holds:
        //   * the two full-root **in-slice** doors (`context::gc::{ensure_heap_full,
        //     alloc_words_rooted}`) publish the FR unconditionally (they pass `true`), and the
        //     narrow door (`ensure_heap`) already ignores the FR entirely — it is gated by the
        //     caller-module float guard, and a non-float module's process that still holds a
        //     live FR (a callee's leftover) is pinned by this very scan;
        //   * `Runtime::serve_gc_request` and `Process::deliver`'s growth cascade can collect a
        //     process the pin does not cover, but such a process's FR is all `NIL` — it was
        //     measured dead by the scan at the end of its last slice (the FR can only be
        //     written while the process runs), and a live one would have been pinned. Appending
        //     `NIL` immediates to that root set is a no-op, so the collection outcome is
        //     identical whether or not the FR words are published.
        let fr_is_live = (0..beam_engine::machine::MAX_FLOAT_REGISTER)
            .any(|index| engine.machine.get_fr(index).is_some_and(|fr| fr != beam_term::NIL));
        if fr_is_live {
            self.runtime.pin_process(pid);
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
        // # Why over-reporting is the safe and faithful direction
        //
        // Because the reservoir is never cleared, re-registering the whole of it on every slice
        // re-lists a payload that `Process::sweep_off_heap_binaries_reachable` pruned at an
        // earlier rooted collection. That costs nothing: `register_off_heap_binary` is
        // dedup-by-`data_ptr` and idempotent (re-registering an existing slot drops the surplus
        // handle), so repeats are cheap and slot-stable. And the over-report itself **mirrors
        // the oracle**, which likewise lists a dropped binary until a GC collects it — so
        // listing a payload the never-cleared reservoir still owns is both the safe and the
        // faithful direction, never a fabricated entry.
        //
        // # Why `Runtime::eval_off_heap` is deliberately NOT registered here
        //
        // That store is the **pid-less** home of the `-eval` root process's escaping payloads,
        // not this machine's work. Attributing it to this process would fabricate a roster entry
        // for a payload the process never built, so it stays out of this seam entirely.
        if let Some(process) = table.get_mut(pid) {
            for payload in engine.machine.refc_binaries() {
                process.register_off_heap_binary(payload.clone());
            }
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
            // heap against the machine's live terms (see `publish_parked_roots`). The FR file
            // is published only when the scan above found it live; the same choice is stored on
            // the engine so the resume scatters exactly this region back.
            engine.fr_published = fr_is_live;
            publish_parked_roots(table, pid, &mut engine.machine, fr_is_live);
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
                // collected over; the publication is what the `Yield` window (never exempt)
                // and the resume's re-sync rely on (see `publish_parked_roots`). The FR file is
                // published only when the scan above found it live, and the same choice is
                // stored on the engine for the resume to replay.
                engine.fr_published = fr_is_live;
                publish_parked_roots(table, pid, &mut engine.machine, fr_is_live);
                // A pending `garbage_collect` request (see `Runtime::request_gc`) is **not**
                // served here: the exemption set above declines it — `serve_gc_request`
                // consults `Process::is_relocation_exempt` before committing to a collection —
                // so the request is consumed and dropped like any other valve-refused one.
                // The admission points and the `Yield` arm are the sites that serve it.
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
