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
//! [`ExecutionContext`] that charges it from the seams the executing machine reaches — the
//! interpreter's **per-instruction** hook ([`reduce`](ExecutionContext::reduce)), the machine's
//! **heap-admission points** ([`ensure_heap`](ExecutionContext::ensure_heap)/
//! [`ensure_heap_full`](ExecutionContext::ensure_heap_full), reached by
//! `test_heap`/`allocate_heap`) and — as a *pricing*, never refusing, charge — the two
//! **call-resolution** seams
//! ([`resolve_external_call`](ExecutionContext::resolve_external_call) for a `call_ext*`,
//! [`resolve_local_call`](ExecutionContext::resolve_local_call) for a local call) plus the BIF
//! dispatch ([`call_bif`](ExecutionContext::call_bif)/
//! [`call_bif_in_machine`](ExecutionContext::call_bif_in_machine)). See
//! [`RuntimeRunner::run`](crate::runner::RuntimeRunner) for how the probe's exhaustion becomes a
//! pre-emption.
//!
//! Every other [`ExecutionContext`] method — the heap, the whole receive/message seam, the local
//! fun/apply seam ([`resolve_local_fun`](ExecutionContext::resolve_local_fun),
//! [`apply_fun`](ExecutionContext::apply_fun)) **and the execution-trace seam**
//! ([`trace_enabled`](ExecutionContext::trace_enabled) /
//! [`trace_event`](ExecutionContext::trace_event)) — is forwarded to the slice's real
//! [`RuntimeContext`] unchanged, so a scheduled process reports `call`/`return_from`/
//! `return_to` events exactly as the single-pass `Runtime::run_*` path does, a
//! `call_ext*` the host can run in-machine is entered in this very slice, and a local call the
//! host serves as a BIF (a `prim_tty` NIF stub) is dispatched rather than jumped into.
//!
//! # The two kinds of charge: **refusing** and **spending**
//!
//! A charge can only end a slice where the engine propagates the seam's error **unchanged**
//! *and* where the machine's recorded position is the instruction the charge belongs to (the
//! runner parks at [`Machine::current_code`](beam_engine::Machine::current_code) and re-enters
//! there). That splits the seams into two kinds:
//!
//! * **Refusing charges** ([`BudgetProbe::charge`]) hand back the typed `preemption: …` error
//!   when the budget is spent, and the runner turns that into
//!   [`RunOutcome::Yield`](beam_process::RunOutcome::Yield). They are taken where the engine
//!   treats a seam error as a *propagating* failure and, crucially, **before** the instruction's
//!   effect: [`reduce`](ExecutionContext::reduce) (charged by `Machine::drive` *after* it has
//!   recorded the instruction as the machine's position) and the two `ensure_heap` doors
//!   (`?`-propagated out of [`Machine::ensure_heap`], reached on the native tier through
//!   `jit_test_heap`, which stores the instruction's position *before* the door —
//!   `jit/baseline/runtime.rs:1442`). Those are the two seams the runner's `Yield` arm is
//!   written for: both leave the machine parked **exactly on** the instruction that was not run.
//! * **Spending charges** ([`BudgetProbe::spend`]) only *price* the work: they consume a unit
//!   when the slice still has one and are a **silent no-op** otherwise — they never return an
//!   error. Three seams are priced this way: the BIF dispatch
//!   ([`call_bif`](ExecutionContext::call_bif)/
//!   [`call_bif_in_machine`](ExecutionContext::call_bif_in_machine)) and the two
//!   **call-resolution** seams ([`resolve_external_call`](ExecutionContext::resolve_external_call)
//!   for a `call_ext*`, [`resolve_local_call`](ExecutionContext::resolve_local_call) for a local
//!   call). For the BIF seam it is deliberately the only safe shape: every engine arm that
//!   dispatches a BIF folds a seam error **into BEAM semantics** — `gc_bif`/`bif` with a fail
//!   label turns it into `Step::Goto(fail)` (`exec_bif`'s guard-failure arm), so a refusing
//!   charge on a guard BIF would take the *fail* path without the BIF ever having run, and a
//!   `call_ext`/`apply` lands it in a live catch frame (`raise_bif_error`). Either way a refused
//!   BIF charge would be a **wrong result**, not a pre-emption. A `call_bif` mistake cannot be
//!   undone by resuming either — see *exactly-once* below.
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
//!
//! # Why a **native** baseline-JIT block is only *priced*, never pre-empted at a call
//!
//! A compiled block runs natively and never crosses the per-instruction hook, and a
//! BIF-and-call-only loop reaches no heap-admission point either, so on the default tier such a
//! loop used to run for free: every BIF dispatch and every call resolution was invisible to the
//! budget, which therefore drained only at an admission door the loop never reached. Pricing
//! those two sites is the C's own rule — in C a call/BIF return consumes reductions and the
//! following `DISPATCH`/`ERTS_BIF_REDS_LEFT` check pre-empts — and it is what makes `used` (and
//! hence `erlang:statistics(reductions)` and `process_info(_, reductions)`) reflect the work the
//! C counts too.
//!
//! Pricing is *not* pre-emption, and the difference matters: the slice of such a loop still ends
//! only at a seam that can safely refuse (an admission door, a `reduce` on an instruction the
//! native tier hands back to the interpreter, or the caller's own hook). The reason is the
//! engine's **position store**, not a preference:
//!
//! * the native tier publishes [`Machine::current_code`](beam_engine::Machine::current_code) at
//!   **block granularity** — the block prologue's `jit_set_position`/`jit_publish_y`
//!   (`jit/baseline/runtime.rs:993`, `:1027`) store the block's *entry* `(func, pc)`, and the
//!   local-call helpers consult the seam **before** any store of their own:
//!   `store_continuation` asks `machine.local_call_seam(...)` at `jit/baseline/call.rs:210` and
//!   only then does `machine.set_current_code(func, pc)` at `:232`; `probe_local_call`
//!   (`jit/baseline/runtime.rs:948`) stores nothing; `resolve_ext_entry` (`:797-849`) asks the
//!   seam at `:818` with the block-entry position still in place;
//! * the runner parks an exhausted machine at `current_code` (`src/runner/run.rs:1162`) and the
//!   driver re-enters at that address from its **block-entry table** first, falling back to a
//!   single interpreter step only for an address no block covers (`jit/baseline/mod.rs:53-54`,
//!   `:125-126`). A block entry is always in that table, so a refusal from one of those two
//!   seams would resume the machine at the *start of the block* and re-execute its prefix — a
//!   second `send`/`put/2`/ETS write for a block that had already performed one. That is a
//!   **wrong result**, not a pre-emption. (Measured, not inferred: with a refusing charge at
//!   those seams, `-eval 'halt(0).'` and `run <fixture> …` both died with `unsupported: opcode:
//!   run: process blocked (no message available)` — a resumed `receive` found the message its
//!   double-executed `send` had already delivered.)
//!
//! The refusal becomes sound as soon as those three sites store the *instruction's* address
//! before consulting the seam (`machine.set_current_code(func, pc)` — exactly what `jit_step_ext`
//! already does at `runtime.rs:574-576` and `jit_test_heap` at `:1442`), because a mid-block
//! address is re-entrant through the interpreter-step fallback. That is a `beam_engine` change
//! (`jit/baseline/{call,runtime}.rs`), so this crate cannot make it; until it lands, the two
//! call-resolution seams and the BIF dispatch **price** their work and the slice ends at the next
//! seam that may refuse.
//!
//! # Priced once per instruction execution
//!
//! A seam is consulted once per **instruction execution**, not twice: the native `call_ext*`
//! helper asks the host seam itself and hands the answer to the dispatching step so the
//! instruction does not ask a second time ([`Machine::call_ext_dispatch`]'s `seam` argument), and
//! a local-call probe that declines jumps straight to the callee instead of re-running the
//! instruction. The BIF seams are mutually exclusive per dispatch too — the engine's
//! `dispatch_bif_call` takes exactly one branch on its `wants_bif_in_machine` latch — so a BIF
//! is never priced twice. The one exception is a **host error/deduction fallback**: `jit_ext_call`
//! leaves an errored seam to the interpreter step, which asks again — the same seam asked twice
//! for one instruction. For a *pricing* charge that is merely conservative (it may price two
//! units where the C counts one); since no charge on that path refuses, it cannot manufacture an
//! exhaustion out of an instruction that never ran twice.
//!
//! # Exactly-once, and why no BIF charge may refuse
//!
//! Every **refusing** seam above is reached **before** its instruction's effect: the `reduce`
//! hook before the body, the heap doors before the allocation. A slice that ends there is resumed
//! at `Machine::current_code` and the instruction runs exactly once, so no work is lost or
//! doubled — and both seams store that address *before* they charge, which is what makes the
//! stored address the instruction itself rather than its block.
//!
//! A BIF dispatch is different: the charge would be taken **around** the dispatch, so a refusal
//! could not be delivered as a clean re-runnable stop. Charging it *before* the dispatch would
//! still be observed by a guard-BIF arm as a guard failure (`Step::Goto(fail)`), i.e. a wrong
//! result; charging it *after* the dispatch and refusing there would leave the BIF already
//! applied — and the surrounding instruction already written its destination — so resuming would
//! re-dispatch a BIF with a visible side effect (`erlang:send/2`, an ETS write, `put/2`). Both
//! are unsound, which is exactly why [`spend`](BudgetProbe::spend) exists: the BIF seam prices
//! the work and the **next** refusing seam ends the slice. A BIF therefore never loses or
//! repeats its effect; the price of the BIF is merely paid into the budget that a later
//! `reduce`/heap-door charge then exhausts.
//!
//! An interpreted instruction that also allocates, calls or dispatches a BIF is consequently
//! priced more than once — harmless, since pre-emption is conservative, and the budget is a
//! *slice* granularity, not a precise per-instruction price.

use std::sync::Arc;

use beam_common::Result;
use beam_engine::context::RootSet;
use beam_engine::{ExecutionContext, Machine};
use beam_loader::{Label, Module};
use beam_term::{Atom, AtomTable, Eterm};

use crate::context::RuntimeContext;

/// The reduction budget of **one scheduler slice**, charged as the machine runs.
///
/// The [`ProcessRunner`](beam_process::ProcessRunner) contract puts the reduction count in
/// the runner's hands (the scheduler never inspects or resets `Process::reductions`), and
/// `beam_engine` exposes no mid-run step counter the runner could read itself, so the count is
/// derived from the seams the executing machine reaches: [`ExecutionContext::reduce`] — called
/// once per interpreted instruction, and therefore the charge that prices an **interpreted** loop
/// with no allocation at all — the two `ensure_heap` doors called by
/// `test_heap`/`allocate_heap`, and, **spend-only**, the two **call-resolution** seams
/// ([`ExecutionContext::resolve_external_call`] for a `call_ext*` and
/// [`ExecutionContext::resolve_local_call`] for a local call) plus the BIF dispatch — which are
/// what price a **native** baseline-JIT block, since it crosses neither of the first two.
///
/// One charge is one reduction-like unit. When the budget runs out, `charge` reports the
/// exhaustion through a typed error; the runner recognises it (the sticky [`exhausted`] flag
/// is set) and turns it into [`RunOutcome::Yield`](beam_process::RunOutcome::Yield) at the
/// machine's recorded address rather than a process failure.
///
/// An exhausted probe is **sticky**: it keeps refusing so a single exhaustion yields exactly
/// once (the first refusal propagates out of the run, so no second charge can occur — the
/// stickiness is what makes the runner's classification unambiguous). Only
/// [`charge`](BudgetProbe::charge) sets it; [`spend`](BudgetProbe::spend) never does.
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
    /// This is the accounting primitive of every **refusing** charging point — the
    /// per-instruction [`reduce`](ExecutionContext::reduce) hook and the two `ensure_heap` doors —
    /// so `used` is always the real work the slice did, whichever seam observed it.
    ///
    /// # Errors
    ///
    /// A typed `unsupported`-prefixed error once the budget is spent — the runner's signal
    /// that the slice must end in [`RunOutcome::Yield`](beam_process::RunOutcome::Yield). It
    /// is **never** a process failure: the runner intercepts it and parks the machine
    /// instead of recording an exit. A seam may only take this charge where the machine's
    /// recorded position is the instruction the charge belongs to (see the module docs).
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

    /// Prices one unit of work **without ever refusing** — the charging primitive of the seams
    /// whose error the engine would neither deliver as a clean, re-runnable stop nor keep out of
    /// BEAM semantics.
    ///
    /// That is the BIF dispatch ([`call_bif`](ExecutionContext::call_bif)/
    /// [`call_bif_in_machine`](ExecutionContext::call_bif_in_machine)) — every engine arm folds a
    /// BIF-seam error into BEAM semantics (a guard failure, or a caught raise), so a refusal
    /// there would be a **wrong result** rather than a pre-emption, and a charge taken around an
    /// already-applied BIF could not be re-run exactly once either — and the two
    /// call-resolution seams ([`resolve_external_call`](ExecutionContext::resolve_external_call)/
    /// [`resolve_local_call`](ExecutionContext::resolve_local_call)), whose refusal would park the
    /// machine at its **block entry** on the native tier rather than at the instruction (see the
    /// module docs). All of them price their work so the budget really drains; the exhaustion is
    /// reported by the next [`charge`](BudgetProbe::charge).
    ///
    /// A spend consumes one unit when the slice still has one and is a **silent no-op** once the
    /// budget is gone: the exhaustion itself is always reported by the **next**
    /// [`charge`](BudgetProbe::charge), which is why `exhausted` is never set here — an
    /// `exhausted` probe with the machine still running would make the runner park a machine that
    /// had walked on (or even finished), and the recorded resume address would then be wrong.
    pub(super) fn spend(&mut self) {
        if self.remaining > 0 {
            self.remaining -= 1;
            self.used += 1;
        }
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
/// [`reduce`](ExecutionContext::reduce) and [`wants_reduce`](ExecutionContext::wants_reduce) are
/// the wrapper's own, because the [`RuntimeContext`] it wraps is a *host* whose `reduce` is the
/// default no-op and whose `wants_reduce` is `false` — the wrapper is the pre-empting host, so it
/// must answer both for itself. The two other **refusing** seams
/// ([`ensure_heap`](ExecutionContext::ensure_heap),
/// [`ensure_heap_full`](ExecutionContext::ensure_heap_full)) charge **around** their forward; the
/// three **pricing** seams — the two call-resolution ones
/// ([`resolve_external_call`](ExecutionContext::resolve_external_call),
/// [`resolve_local_call`](ExecutionContext::resolve_local_call)) and the BIF dispatch
/// ([`call_bif`](ExecutionContext::call_bif)/
/// [`call_bif_in_machine`](ExecutionContext::call_bif_in_machine)) — price their forwards with the
/// non-refusing [`BudgetProbe::spend`].
///
/// Everything else — the heap, the local fun/apply seam, the receive seam **and the
/// execution-trace seam** — is forwarded unchanged; the redirect is purely mechanical.
pub(super) struct BudgetContext<'c, 'a> {
    /// The slice's real context.
    pub(super) inner: &'c mut RuntimeContext<'a>,
    /// The slice's budget.
    pub(super) probe: &'c mut BudgetProbe,
}

impl ExecutionContext for BudgetContext<'_, '_> {
    /// Serve the BIF, then **price** it with one reduction unit
    /// ([`BudgetProbe::spend`] — never a refusal: see the module docs for why a BIF-seam error
    /// cannot be a pre-emption and why charging after the dispatch is the only exactly-once
    /// shape).
    ///
    /// The BIF's own error is propagated **unchanged** and is not priced: an erroring BIF ends
    /// the process (or lands a raise) and its unit is not worth accounting.
    ///
    /// This is not the only BIF seam of a slice's machine — the engine prefers
    /// [`call_bif_in_machine`](ExecutionContext::call_bif_in_machine) whenever the host opts in,
    /// which [`RuntimeContext`] does — so both are priced, and they are mutually exclusive per
    /// dispatch (one BIF is never charged twice).
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let value = ExecutionContext::call_bif(self.inner, module, function, arity, args)?;
        self.probe.spend();
        Ok(value)
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

    /// Forwarded like every other method — and **load-bearing** for the scheduled path, because
    /// the slice's machine runs on this wrapper and therefore reaches the entry-aware dispatch
    /// here, never on the inner context directly.
    ///
    /// Priced exactly like [`call_bif`](ExecutionContext::call_bif): a **spend**, after the
    /// dispatch, never a refusal. This is the seam the engine's `dispatch_bif_call` actually
    /// takes (the host opts into the in-machine door), so without this pair the slice would
    /// price no BIF dispatch at all.
    fn call_bif_in_machine(
        &mut self,
        machine: &Machine,
        entry_module: &Module,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let value = ExecutionContext::call_bif_in_machine(
            self.inner,
            machine,
            entry_module,
            module,
            function,
            arity,
            args,
        )?;
        self.probe.spend();
        Ok(value)
    }
    /// **Price one reduction per `call_ext*` resolution**, then forward the resolution.
    ///
    /// This is one of the two pricing seams that make a **native** baseline-JIT block's budget
    /// drain: a compiled block crosses neither the per-instruction hook nor (for a
    /// BIF-and-call-only loop) a heap-admission point, so before this price a `call_ext`-driven
    /// loop ran its whole slice for free. In C a call's return consumes reductions and the
    /// following `DISPATCH` pre-empts; this is the same unit, charged at the resolution.
    ///
    /// # Why it *prices* rather than refuses
    ///
    /// `Machine::resolve_external_entry` **`?`-propagates** this seam's error, and it reaches the
    /// seam *before* any continuation, frame, module context or heap write — so the instruction
    /// itself would be re-runnable. The machine's **recorded position**, however, would not be:
    /// the native tier stores a block's *entry* `(func, pc)` in the block prologue and
    /// `resolve_ext_entry` asks this seam (`jit/baseline/runtime.rs:818`) with that entry still in
    /// place, so a refusal would park the runner at the block entry (a real compiled-block
    /// address) and the resume would re-execute the block's prefix — repeating any `send`/`put/2`
    /// it already performed. See the module docs; the instruction-exact store that makes a
    /// refusal sound is a `beam_engine` change.
    ///
    /// The price is still worth taking: it is what makes `used` (and
    /// `erlang:statistics(reductions)`) count the call, and it is what lets the **next** refusing
    /// seam end the slice at a safe address.
    ///
    /// Still **load-bearing** beyond the price: the slice's machine runs on *this* wrapper, so
    /// without the forward the engine would never see the real context's cross-module-call seam
    /// (`RuntimeContext::resolve_external_call`) and a `call_ext*` would always take the
    /// synchronous `call_bif` route — a second `Machine` whose GC admission point is never armed,
    /// so the callee's allocations fall back to the runtime-wide arena and the process's peak
    /// memory grows with the loop count. Forwarding lets the engine `Step::Enter` the callee
    /// **inside this same machine**, whose [`ensure_heap`](ExecutionContext::ensure_heap) below
    /// charges the budget *and* arms the collection latch for it.
    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        self.probe.spend();
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

    /// **Price one reduction per local-call resolution**, then forward the resolution.
    ///
    /// The same native-tier pricing as
    /// [`resolve_external_call`](ExecutionContext::resolve_external_call), for the loop shape
    /// that calls instead of calling out: a tail-recursive counter whose work is `bif` arithmetic
    /// plus a local `call_only`/`call_last` reaches **no** heap-admission point and no
    /// `call_ext`, so this is the only seam such a loop crosses that observes its work at all. C
    /// charges a reduction per call; this is the same unit.
    ///
    /// # Why it *prices* rather than refuses
    ///
    /// `Machine::local_call_seam` **`?`-propagates** this seam's error on all three interpreted
    /// local-call arms, and it asks the seam *before* the continuation is stored / the frame is
    /// popped. On the native tier, though, the two helpers that reach it ask it with the
    /// **block-entry** position still current (`store_continuation` asks at
    /// `jit/baseline/call.rs:210` and stores the call site's address only afterwards at `:232`;
    /// `probe_local_call` at `jit/baseline/runtime.rs:948` stores nothing), so a refusal would
    /// park the machine at the block entry and the resume would re-execute the block's prefix —
    /// a second side effect, not a pre-emption. See the module docs; the instruction-exact store
    /// that makes a refusal sound is a `beam_engine` change.
    ///
    /// A probe that *declines* (`Ok(false)`) is unaffected either way: the arm/block jumps
    /// straight to the callee instead of re-running the instruction, so the seam is asked once
    /// per executed local call and priced once.
    ///
    /// Still **load-bearing** beyond the price: the slice's machine runs on *this* wrapper, so
    /// the engine's three local-call arms (`Call`/`CallOnly`/`CallLast`) and the native baseline
    /// tier's `jit_local_call` probe consult the local-call seam here: without the forward they
    /// would see the trait default (`Ok(false)`), always decline, and jump **in-machine** into a
    /// non-exported same-module callee's own code — which for a built-in/static NIF library stub
    /// such as `prim_tty:tty_create/1` (body `erlang:nif_error(undef)`) raises `undef` instead of
    /// reaching the host that serves it. Forwarding reaches
    /// [`RuntimeContext`]'s own override (`crate::native::prim_tty::serves_local_call`), so the
    /// served key is dispatched through [`call_bif`](ExecutionContext::call_bif) under the arm's
    /// frame discipline and every other key stays the unchanged in-machine jump.
    fn resolve_local_call(&mut self, module: &str, function: &str, arity: u32) -> Result<bool> {
        self.probe.spend();
        ExecutionContext::resolve_local_call(self.inner, module, function, arity)
    }

    /// Forwarded like every other method (see
    /// [`resolve_external_call`](ExecutionContext::resolve_external_call) for which methods the
    /// wrapper itself overrides) — and **load-bearing** for the scheduled path.
    ///
    /// The slice's machine runs on *this* wrapper, so the engine resolves a runtime atom value
    /// (a variable `M`/`F` in `apply(M,F,A)` / `M:F(...)`) here: without the forward the trait
    /// default would index only the running module's load-time snapshot, so a module loaded
    /// early could not name an atom interned later. That is exactly the A.9 boot shape —
    /// `gen_server:init_it/6` evaluates `{ok, Mod:init(Args)}` with a `Mod` interned after
    /// `gen_server` was loaded — so forwarding reaches
    /// [`RuntimeContext`]'s own override (the runtime's live shared namespace) and the resolution
    /// succeeds instead of raising `badarg`.
    fn atom_name(&self, module: &Module, atom: Atom) -> Option<String> {
        ExecutionContext::atom_name(self.inner, module, atom)
    }

    /// Forwarded like every other method (see
    /// [`resolve_external_call`](ExecutionContext::resolve_external_call) for which methods the
    /// wrapper itself overrides) — and **load-bearing** for the scheduled path, for the same
    /// reason as [`atom_name`](ExecutionContext::atom_name): the slice's machine runs on *this*
    /// wrapper, so the engine's `test`/`select_val`/map-key comparisons take the namespace here.
    /// Without the forward they would see the trait default (`&module.atom_table`, the module's
    /// load-time snapshot) and a module loaded early could not name an atom interned later — the
    /// `sofs:drestriction/2` shape, whose `{module_info,0} < {g,1}` guard was then ordered by raw
    /// atom id instead of by name and took the wrong `sofs:restrict_n/5` clause. Forwarding
    /// reaches [`RuntimeContext`]'s own override (the runtime's live shared namespace), so every
    /// comparison orders atoms by name exactly as the `<`/`==` BIFs do.
    fn atoms<'m>(&'m self, module: &'m Module) -> &'m AtomTable {
        ExecutionContext::atoms(self.inner, module)
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
    /// reaches, so the collection it may drive sees each register file up to its pointer-bearing
    /// prefix **and the whole Erlang stack**. The latch is armed for the slice's own machine only
    /// (see the type docs), which is exactly the root set
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
