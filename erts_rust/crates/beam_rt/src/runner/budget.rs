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

//! The **reduction budget** of one scheduler slice and the slice context that charges it.
//!
//! [`BudgetProbe`] counts the reduction units a slice spends; [`BudgetContext`] is the wrapper
//! [`ExecutionContext`] that charges it from the two seams the executing machine reaches — the
//! interpreter's **per-instruction** hook ([`reduce`](ExecutionContext::reduce)) and the
//! machine's **heap-admission points** ([`ensure_heap`](ExecutionContext::ensure_heap)/
//! [`ensure_heap_full`](ExecutionContext::ensure_heap_full), reached by
//! `test_heap`/`allocate_heap`). See
//! [`RuntimeRunner::run`](crate::runner::RuntimeRunner) for how the probe's exhaustion becomes a
//! pre-emption.
//!
//! Every other [`ExecutionContext`] method — BIF/apply dispatch, the heap, the whole
//! receive/message seam, the cross-module-call seam
//! ([`resolve_external_call`](ExecutionContext::resolve_external_call)) **and the
//! execution-trace seam** ([`trace_enabled`](ExecutionContext::trace_enabled) /
//! [`trace_event`](ExecutionContext::trace_event)) — is forwarded to the slice's real
//! [`RuntimeContext`] unchanged, so a scheduled process reports `call`/`return_from`/
//! `return_to` events exactly as the single-pass `Runtime::run_*` path does and a
//! `call_ext*` the host can run in-machine is entered in this very slice.
//!
//! # Why charging the per-instruction hook is safe
//!
//! `Machine::drive` prices an interpreted instruction **before** its body runs: it records the
//! machine's position as that instruction (the address
//! [`Machine::current_code`](beam_engine::Machine::current_code) hands back) and only then calls
//! the hook. An `Err` from [`reduce`](ExecutionContext::reduce) therefore ends the run with the
//! instruction's effect **not yet applied** and the machine parked exactly on it, so
//! [`Machine::resume_outcome`](beam_engine::Machine::resume_outcome) applies it **exactly once**
//! — the same guarantee C's `DISPATCH` gives by checking and consuming `FCALLS` before the body.
//! A loop that allocates nothing (`test_heap` is unreachable from it, and a `gc_bif` reaches
//! only [`call_bif`](ExecutionContext::call_bif)) is thereby pre-emptible, which is precisely
//! what a heap-admission charge alone cannot do.
//!
//! The two `ensure_heap` doors stay charged as well: they are the **only** charge a native
//! baseline-JIT block reaches, because a compiled block runs natively and never crosses the
//! per-instruction hook (see
//! [`RuntimeRunner::run`](crate::runner::RuntimeRunner) for the residual limit that leaves on
//! that tier). An interpreted instruction that also allocates is consequently charged twice —
//! harmless, since pre-emption is conservative, and it leaves the native tier's fairness
//! unchanged.

use std::sync::Arc;

use beam_common::Result;
use beam_engine::context::RootSet;
use beam_engine::{ExecutionContext, Machine};
use beam_loader::{Label, Module};
use beam_term::Eterm;

use crate::context::RuntimeContext;

/// The reduction budget of **one scheduler slice**, charged as the machine runs.
///
/// The [`ProcessRunner`](beam_process::ProcessRunner) contract puts the reduction count in
/// the runner's hands (the scheduler never inspects or resets `Process::reductions`), and
/// `beam_engine` exposes no mid-run step counter the runner could read itself, so the count is
/// derived from the two seams the executing machine reaches:
/// [`ExecutionContext::reduce`] — called once per interpreted instruction, and therefore the
/// charge that prices a loop with **no** allocation at all — and
/// [`ExecutionContext::ensure_heap`], called by `test_heap`/`allocate_heap` (the only charge a
/// native baseline-JIT block reaches).
///
/// One charge is one reduction-like unit. When the budget runs out, `charge` reports the
/// exhaustion through a typed error; the runner recognises it (the sticky [`exhausted`] flag
/// is set) and turns it into [`RunOutcome::Yield`](beam_process::RunOutcome::Yield) at the
/// machine's recorded address rather than a process failure.
///
/// An exhausted probe is **sticky**: it keeps refusing so a single exhaustion yields exactly
/// once (the first refusal propagates out of the run, so no second charge can occur — the
/// stickiness is what makes the runner's classification unambiguous).
///
/// [`exhausted`]: BudgetProbe::is_exhausted
pub(super) struct BudgetProbe {
    /// The reduction units left in this slice. Never zero: a degenerate `budget` of 0 still
    /// gets one unit, so a slice always makes progress and the scheduler's run-queue loop
    /// cannot spin forever on an immediately-yielding process.
    remaining: u32,
    /// The reduction units charged so far (what the process's counter is set to).
    used: u32,
    /// Whether the budget ran out during this slice.
    exhausted: bool,
}

impl BudgetProbe {
    /// A probe with `budget` reduction units (at least one).
    pub(super) fn new(budget: u32) -> BudgetProbe {
        BudgetProbe {
            remaining: budget.max(1),
            used: 0,
            exhausted: false,
        }
    }

    /// The reduction units this slice charged.
    pub(super) fn used(&self) -> u32 {
        self.used
    }

    /// Whether the budget ran out during this slice.
    pub(super) fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Charges one reduction unit, or reports the exhaustion.
    ///
    /// This is the single accounting primitive of **both** of the slice's charging points — the
    /// per-instruction [`reduce`](ExecutionContext::reduce) hook and the two `ensure_heap`
    /// doors — so `used` is always the real work the slice did, whichever seam observed it.
    ///
    /// # Errors
    ///
    /// A typed `unsupported`-prefixed error once the budget is spent — the runner's signal
    /// that the slice must end in [`RunOutcome::Yield`](beam_process::RunOutcome::Yield). It
    /// is **never** a process failure: the runner intercepts it and parks the machine
    /// instead of recording an exit.
    pub(super) fn charge(&mut self) -> Result<()> {
        if self.remaining == 0 {
            self.exhausted = true;
            return Err(crate::error::unsupported(
                "preemption: the reduction budget for this scheduler slice is spent",
            ));
        }
        self.remaining -= 1;
        self.used += 1;
        Ok(())
    }
}

/// The **slice's** context: the [`RuntimeContext`] the executing machine runs on, with the
/// slice's reduction budget charged from the engine's per-instruction hook and its
/// heap-admission points.
///
/// This wrapper exists because `beam_rt` may not change `beam_engine` or its own
/// `src/context/**`: it is handed to the machine the runner drives, and every context the
/// engine builds *inside* that run — an external module call
/// ([`RuntimeContext::run_export`]) or a fun application — is a plain `RuntimeContext`, so
/// the wrapper's charging is reached by the slice's own machine **only**. That is what makes
/// the recorded resume address exact and keeps a nested run from being mistaken for the
/// pre-empted one (nor from consuming the budget).
///
/// The two charging seams are declared here rather than forwarded:
/// [`reduce`](ExecutionContext::reduce) and [`wants_reduce`](ExecutionContext::wants_reduce)
/// are the wrapper's own, because the [`RuntimeContext`] it wraps is a
/// *host* whose `reduce` is the default no-op and whose `wants_reduce` is `false` — the
/// wrapper is the pre-empting host, so it must answer both for itself.
///
/// Everything else — the BIF/apply dispatch, the heap, the receive seam **and the
/// execution-trace seam** — is forwarded unchanged; the redirect is purely mechanical.
pub(super) struct BudgetContext<'c, 'a> {
    /// The slice's real context.
    pub(super) inner: &'c mut RuntimeContext<'a>,
    /// The slice's budget.
    pub(super) probe: &'c mut BudgetProbe,
}

impl ExecutionContext for BudgetContext<'_, '_> {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        ExecutionContext::call_bif(self.inner, module, function, arity, args)
    }

    /// Forwarded like every other method — and **load-bearing** for the scheduled path.
    ///
    /// The slice's machine runs on *this* wrapper, so the engine's once-per-run latch
    /// ([`Machine::dispatch_bif_call`](beam_engine::Machine)) reads the probe **here**:
    /// without this override it would see the trait default (`false`), the engine would
    /// never take the in-machine door, and a scheduled process could not answer its own
    /// `erlang:process_info(self(), current_stacktrace)` — `gen_server`'s `?STACKTRACE()`.
    fn wants_bif_in_machine(&self) -> bool {
        ExecutionContext::wants_bif_in_machine(self.inner)
    }

    /// Forwarded like every other method (see
    /// [`resolve_external_call`](ExecutionContext::resolve_external_call) for which methods the
    /// wrapper itself overrides) — and **load-bearing** for the scheduled path, because the
    /// slice's machine runs on this wrapper and therefore reaches the entry-aware dispatch
    /// here, never on the inner context directly.
    fn call_bif_in_machine(
        &mut self,
        machine: &Machine,
        entry_module: &Module,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        ExecutionContext::call_bif_in_machine(
            self.inner,
            machine,
            entry_module,
            module,
            function,
            arity,
            args,
        )
    }
    /// Forwarded like every other method (the wrapper's own overrides are
    /// [`reduce`](ExecutionContext::reduce)/[`wants_reduce`](ExecutionContext::wants_reduce)
    /// and the two `ensure_heap` doors).
    ///
    /// This one is **load-bearing** rather than merely faithful: the slice's machine runs on
    /// *this* wrapper, so without the forward the engine would never see the real context's
    /// cross-module-call seam (`RuntimeContext::resolve_external_call`) and a `call_ext*`
    /// would always take the synchronous `call_bif` route — a second `Machine` whose GC
    /// admission point is never armed, so the callee's allocations fall back to the
    /// runtime-wide arena and the process's peak memory grows with the loop count. Forwarding
    /// lets the engine `Step::Enter` the callee **inside this same machine**, whose
    /// [`ensure_heap`](ExecutionContext::ensure_heap) above charges the budget *and* arms the
    /// collection latch for it.
    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        ExecutionContext::resolve_external_call(self.inner, module, function, arity)
    }

    /// Forwarded like every other method (see
    /// [`resolve_external_call`](ExecutionContext::resolve_external_call) for which methods the
    /// wrapper itself overrides) — and **load-bearing** for the scheduled path.
    ///
    /// The slice's machine runs on *this* wrapper, so the engine's `CallFun`/`CallFun2` arms
    /// consult the closure resolver here: without the forward the engine would see the trait
    /// default (`Ok(None)`) and fold a blocking local closure into the value-returning
    /// [`apply_fun`](ExecutionContext::apply_fun) fallback, whose nested machine may not re-enter
    /// the scheduler. Forwarding lets the engine `Step::Enter` the closure body **inside this
    /// same machine**, so a `receive` in it suspends the process through the ordinary
    /// [`RunOutcome::Wait`](beam_process::RunOutcome::Wait) path and it is resumed by a real send.
    fn resolve_local_fun(
        &mut self,
        fun: Eterm,
        args: &[Eterm],
    ) -> Result<Option<(Arc<Module>, Label)>> {
        ExecutionContext::resolve_local_fun(self.inner, fun, args)
    }

    /// Charge one reduction, then serve the request by delegating — **arming the GC latch**
    /// for that one admission point first.
    ///
    /// This wrapper's [`ensure_heap`](ExecutionContext::ensure_heap) is reached by the
    /// slice's **own** machine and by nothing else (see the type docs), which is exactly the
    /// root set [`crate::context::gc::ensure_heap`] may collect against; the latch is what
    /// tells it so, and it is cleared as the first statement there (see
    /// [`RuntimeContext::arm_gc`]).
    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.probe.charge()?;
        self.inner.arm_gc();
        ExecutionContext::ensure_heap(self.inner, heap_need, live, x_regs, stack)
    }

    /// The **primary** admission point (`test_heap`/`allocate_heap`): charge one reduction,
    /// arm the GC latch, then serve the request over the machine's **whole** root set.
    ///
    /// This is the door [`Machine::ensure_heap`](beam_engine::Machine::ensure_heap) actually
    /// reaches, so the collection it may drive sees every x register, the whole Erlang stack
    /// **and the whole FR file**. The latch is armed for the slice's own machine only (see the
    /// type docs), which is exactly the root set
    /// [`crate::context::gc::ensure_heap_full`] may collect against; it is cleared as the first
    /// statement there (see [`RuntimeContext::arm_gc`]).
    fn ensure_heap_full(&mut self, heap_need: u32, roots: RootSet<'_>) -> Result<()> {
        self.probe.charge()?;
        self.inner.arm_gc();
        ExecutionContext::ensure_heap_full(self.inner, heap_need, roots)
    }

    /// Charge the slice's budget **once per interpreted instruction** — the pre-emption seam a
    /// non-allocating loop is reachable from.
    ///
    /// The engine's driver calls this before each instruction's body, which is what makes the
    /// pre-emption exact: an `Err` here leaves the instruction un-applied and the machine parked
    /// on it (see the module docs). The charge is `n` units — the interpreter always passes `1`,
    /// and the loop keeps the accounting faithful for any other count — and an exhausted probe's
    /// typed error is returned **unchanged**, so the runner's sticky-`exhausted` classification
    /// keeps working.
    ///
    /// This is **not** forwarded to `inner`: the wrapped [`RuntimeContext`] is the host of the
    /// single-pass path, whose `reduce` is the default no-op. The wrapper is the pre-empting
    /// host of a scheduler slice, so it charges here itself.
    fn reduce(&mut self, n: u32) -> Result<()> {
        for _ in 0..n {
            self.probe.charge()?;
        }
        Ok(())
    }

    /// The load-bearing declaration that **this** host pre-empts.
    ///
    /// The engine's contract is that a host may answer `false` only when its `reduce` is
    /// provably the default no-op (it then skips the per-instruction vtable crossing). A
    /// scheduler slice always charges, so the wrapper must answer `true` explicitly and must
    /// **not** forward the question to `inner` — the wrapped [`RuntimeContext`] answers `false`
    /// for the single-pass path, and inheriting that here would silently disable every
    /// per-instruction charge and re-open the non-allocating-loop gap.
    fn wants_reduce(&self) -> bool {
        true
    }
    fn heap_available(&self) -> usize {
        ExecutionContext::heap_available(&*self.inner)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        ExecutionContext::alloc_words(self.inner, n)
    }

    /// The engine's raw-word allocation ([`Machine::alloc_rooted`](beam_engine::Machine::alloc_rooted),
    /// reached by `terms::reserve`/`reserve_slack`): arm the GC latch and forward.
    ///
    /// The budget is **not** charged here: this is not one of the slice's charging seams (the
    /// per-instruction [`reduce`](ExecutionContext::reduce) hook and the two `ensure_heap`
    /// doors). Arming the latch is still required: this is a legitimate collection site for the
    /// slice's own machine (it is handed the whole root set), and without the latch the nested
    /// machinery would refuse to collect.
    fn alloc_words_rooted(
        &mut self,
        n: usize,
        roots: RootSet<'_>,
    ) -> Result<*mut Eterm> {
        self.inner.arm_gc();
        ExecutionContext::alloc_words_rooted(self.inner, n, roots)
    }

    fn apply(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        ExecutionContext::apply(self.inner, module, function, arity, args)
    }

    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        ExecutionContext::apply_fun(self.inner, fun, args)
    }

    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        ExecutionContext::send_message(self.inner, to, message)
    }

    /// Forwarded like [`send_message`](ExecutionContext::send_message) — and **load-bearing** for
    /// the scheduler path.
    ///
    /// The slice's machine runs on *this* wrapper, so the engine's `send` arm calls
    /// `send_message_with_off_heap` on the wrapper: without this forward the **default** body would
    /// run, forwarding to `send_message` and **discarding** `src_binaries`, so the receiver-side
    /// copy could not resolve — and therefore could not **share** — an engine-built off-heap refc
    /// binary in the message; it would flatten it on-heap instead. That is the send-path half of
    /// the transport bug, reachable only from a scheduled process. Forwarding the slice reaches
    /// [`RuntimeContext`]'s own override.
    fn send_message_with_off_heap(
        &mut self,
        to: Eterm,
        message: Eterm,
        src_binaries: &[beam_alloc::RefcBinary],
    ) -> Result<()> {
        ExecutionContext::send_message_with_off_heap(self.inner, to, message, src_binaries)
    }
    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        ExecutionContext::receive_next(self.inner)
    }

    fn receive_skip(&mut self) -> Result<()> {
        ExecutionContext::receive_skip(self.inner)
    }

    fn receive_remove(&mut self) -> Result<()> {
        ExecutionContext::receive_remove(self.inner)
    }

    fn receive_reset(&mut self) -> Result<()> {
        ExecutionContext::receive_reset(self.inner)
    }

    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        ExecutionContext::receive_marker_reserve(self.inner)
    }

    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        ExecutionContext::receive_marker_bind(self.inner, marker, reference)
    }

    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        ExecutionContext::receive_marker_use(self.inner, reference)
    }

    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        ExecutionContext::receive_marker_clear(self.inner, reference)
    }

    /// Forwarded like every other method (see
    /// [`resolve_external_call`](ExecutionContext::resolve_external_call) for which methods the
    /// wrapper itself overrides): the slice's machine latches this once per run, so a
    /// **scheduled** process reports `call`/`return_from`/`return_to` exactly as the
    /// single-pass `Runtime::run_*` path does.
    #[inline]
    fn trace_enabled(&self) -> bool {
        ExecutionContext::trace_enabled(self.inner)
    }

    /// Forwarded like every other method (see [`trace_enabled`](ExecutionContext::trace_enabled)):
    /// the slice's own context decides matching and delivers the `{trace, …}` message.
    fn trace_event(&mut self, event: beam_engine::TraceEvent<'_>) {
        ExecutionContext::trace_event(self.inner, event)
    }
}
