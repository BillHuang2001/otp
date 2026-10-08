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
//! [`BudgetProbe`] counts the reduction units a slice spends at the machine's
//! heap-admission points; [`BudgetContext`] is the wrapper [`ExecutionContext`] that
//! redirects an [`ensure_heap`](ExecutionContext::ensure_heap) on the slice's own machine
//! into that probe. See [`RuntimeRunner::run`](crate::runner::RuntimeRunner) for how the
//! probe's exhaustion becomes a pre-emption.
//!
//! Every other [`ExecutionContext`] method — BIF/apply dispatch, the heap, the whole
//! receive/message seam, the cross-module-call seam
//! ([`resolve_external_call`](ExecutionContext::resolve_external_call)) **and the
//! execution-trace seam** ([`trace_enabled`](ExecutionContext::trace_enabled) /
//! [`trace_event`](ExecutionContext::trace_event)) — is forwarded to the slice's real
//! [`RuntimeContext`] unchanged, so a scheduled process reports `call`/`return_from`/
//! `return_to` events exactly as the single-pass `Runtime::run_*` path does and a
//! `call_ext*` the host can run in-machine is entered in this very slice.

use std::sync::Arc;

use beam_common::Result;
use beam_engine::ExecutionContext;
use beam_loader::{Label, Module};
use beam_term::Eterm;

use crate::context::RuntimeContext;

/// The reduction budget of **one scheduler slice**, charged as the machine runs.
///
/// The [`ProcessRunner`](beam_process::ProcessRunner) contract puts the reduction count in
/// the runner's hands (the scheduler never inspects or resets `Process::reductions`), and
/// `beam_engine` exposes no mid-run reduction/step counter, so the count is derived from
/// the one per-instruction seam the executing machine reaches on its own:
/// [`ExecutionContext::ensure_heap`], called by `test_heap`/`allocate_heap` — the
/// instructions the compiler emits once per loop iteration that allocates (the ordinary
/// Erlang shape).
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
/// slice's reduction budget charged at its heap-admission points.
///
/// This wrapper exists because `beam_rt` may not change `beam_engine` or its own
/// `src/context/**`: it is handed to the machine the runner drives, and every context the
/// engine builds *inside* that run — an external module call
/// ([`RuntimeContext::run_export`]) or a fun application — is a plain `RuntimeContext`, so
/// the wrapper's [`ensure_heap`](ExecutionContext::ensure_heap) is reached by the slice's own
/// machine **only**. That is what makes the recorded resume address exact and keeps a nested
/// run from being mistaken for the pre-empted one.
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

    /// Forwarded like every other method (the wrapper redirects only
    /// [`ensure_heap`](ExecutionContext::ensure_heap)).
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

    fn heap_available(&self) -> usize {
        ExecutionContext::heap_available(&*self.inner)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        ExecutionContext::alloc_words(self.inner, n)
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

    /// Forwarded like every other method (the wrapper redirects only
    /// [`ensure_heap`](ExecutionContext::ensure_heap)): the slice's machine latches this once
    /// per run, so a **scheduled** process reports `call`/`return_from`/`return_to` exactly as
    /// the single-pass `Runtime::run_*` path does.
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
