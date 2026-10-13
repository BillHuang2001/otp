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

//! The [`RuntimeContext`] type: the runtime borrowed for one run, plus the output sink, the
//! mailbox identity, the caller-module name, the slice's engines and the per-call GC latch
//! and dispatch memo it carries.
//!
//! Everything else about the context lives in this module's siblings — [`crate::context`]
//! maps them.

use beam_bifs::OutputSink;
use beam_common::{BeamError, Result};
use beam_engine::context::RootSet;
use beam_engine::Machine;
use beam_loader::Module;
use beam_process::{ProcessId, ProcessTable};

use crate::runner::RunnerState;
use crate::runtime::Runtime;

use super::bridges::SharedSink;
use super::dispatch::DispatchMemo;
use super::gc;

/// The engine's host: the runtime, borrowed for the duration of one run, plus the
/// output sink that run writes to.
///
/// A `RuntimeContext` with no sink ([`RuntimeContext::new`]) writes `io:format` output
/// to the process's stdout (`beam_bifs`' default); [`RuntimeContext::with_output`]
/// redirects it — that is how [`Runtime::run_module`] captures output.
pub struct RuntimeContext<'a> {
    /// The runtime: registry, shared atom table, heaps, pid, ETS tables.
    ///
    /// `pub(crate)` for the runtime-native BIF floor ([`crate::native`]), which is a
    /// sibling module of this one and therefore needs explicit visibility.
    pub(crate) runtime: &'a mut Runtime,
    /// The `io:format`/`io:put_chars`/`display` destination; stdout when unset.
    ///
    /// A **shared** handle (see [`SharedSink`]) so the *same* sink can be handed to a
    /// nested context when a `fun`-taking BIF applies its fun ([`FunBridge`]).
    pub(crate) output: Option<SharedSink<'a>>,
    /// The process the context runs **as** — the mailbox owner, and the pid
    /// `erlang:self/0` reports.
    ///
    /// It is the runtime's own [`self_pid`](Runtime::self_pid) unless the context was
    /// built with [`RuntimeContext::for_process`].
    pub(super) mailbox_pid: ProcessId,
    /// A **caller-supplied** process table to resolve that process against, if any.
    ///
    /// `None` means "use the runtime's own [`processes`](Runtime::processes)". The
    /// scheduler pass runs a process whose `Process` lives in a `ProcessTable` it owns
    /// separately from `&mut Runtime` (it cannot hand the same table to both), so the
    /// context must be able to resolve a mailbox against either source — see
    /// [`RuntimeContext::for_process`].
    pub(super) mailbox_table: Option<&'a mut ProcessTable>,
    /// Whether this context may advance the scheduler itself.
    ///
    /// The `-eval` evaluator's `receive` has no engine-level blocking: when a message is
    /// missing it *pumps* the scheduler ([`Runtime::pump_scheduler`]) so a process
    /// `erlang:spawn/1` started runs and can deliver. A context created by
    /// [`RuntimeContext::for_process`] is itself **inside** a scheduler slice, so it must
    /// never re-enter the scheduler (that would run a second process from within a slice);
    /// it reports the typed capability gap instead.
    ///
    /// [`Runtime::pump_scheduler`]: crate::runtime::Runtime::pump_scheduler
    pub(super) may_pump_scheduler: bool,
    /// The name of the module whose **code** is executing — the call site of a BIF.
    ///
    /// [`ExecutionContext::call_bif`] only supplies the *callee* module, but
    /// `erlang:load_nif/2` must know the module the **call site** lives in (the oracle derives
    /// it from the call instruction). [`RuntimeContext::run_export`] is the one place that
    /// knows it, so it sets this for the duration of the call and restores the previous value
    /// afterwards; [`RuntimeContext::call_registered`] forwards it to the [`BifContext`].
    pub(super) current_module: Option<String>,
    /// The engines of the scheduler pass this context is a slice of, when it is one.
    ///
    /// `Some` exactly for a context built by
    /// [`RuntimeContext::for_process_with_engines`] — the runner's slice. It is the
    /// read-only half of the split borrow the pass performs: the runner holds the process
    /// table and drives the executing process's engine (which it has taken **out** of the
    /// map), while this context reads every *other* process's engine from here (see
    /// [`crate::runner::RunnerState::engine_at`] and [`crate::native::memory`], the one
    /// consumer — the `process_info/2` execution-position items).
    ///
    /// A context **outside** a pass ([`RuntimeContext::new`]/[`RuntimeContext::with_output`])
    /// leaves it `None`: those engines are still in the runtime
    /// ([`Runtime::runner_state`](crate::runtime::Runtime)), which such a context reads
    /// directly.
    pub(super) engines: Option<&'a RunnerState>,
    /// The **per-call GC latch**: whether the next
    /// [`ensure_heap`](ExecutionContext::ensure_heap) on this context may collect the running
    /// process's heap.
    ///
    /// It is `true` for exactly one admission point — the slice's own machine, whose
    /// [`BudgetContext::ensure_heap`](crate::runner) arms it **immediately before** delegating
    /// — and is [`take`](std::mem::take)n as the first statement of
    /// [`gc::ensure_heap`](crate::context::gc::ensure_heap), so it can never leak into a
    /// *nested* run on the same context (an external module call's new machine, a fun a BIF
    /// applied, the `-eval` evaluator), whose roots a collection driven from this context
    /// would corrupt (see [`crate::context::gc`]). It is never set for a plain
    /// [`RuntimeContext`].
    pub(super) gc_armed: bool,
    /// The memo of [`RuntimeContext::dispatch`] answers, so a hot call site pays the dispatch
    /// prologue once instead of once per call (see [`DispatchMemo`] for the soundness argument).
    pub(super) dispatch_memo: DispatchMemo,
}

impl<'a> RuntimeContext<'a> {
    /// A context over `runtime`, with the default (stdout) output sink, running as the
    /// runtime's own process.
    pub fn new(runtime: &'a mut Runtime) -> RuntimeContext<'a> {
        let mailbox_pid = runtime.self_pid;
        RuntimeContext {
            runtime,
            output: None,
            mailbox_pid,
            mailbox_table: None,
            may_pump_scheduler: true,
            current_module: None,
            engines: None,
            gc_armed: false,
            dispatch_memo: DispatchMemo::default(),
        }
    }

    /// A context over `runtime` whose program output goes to `sink`.
    pub fn with_output(runtime: &'a mut Runtime, sink: &'a mut dyn OutputSink) -> RuntimeContext<'a> {
        let mailbox_pid = runtime.self_pid;
        RuntimeContext {
            runtime,
            output: Some(SharedSink::new(sink)),
            mailbox_pid,
            mailbox_table: None,
            may_pump_scheduler: true,
            current_module: None,
            engines: None,
            gc_armed: false,
            dispatch_memo: DispatchMemo::default(),
        }
    }

    /// A context that runs **as `pid`**, resolving that process and its mailbox against
    /// the caller-supplied `table` instead of the runtime's own
    /// [`processes`](Runtime::processes).
    ///
    /// This is the seam the scheduler pass builds on: a `ProcessRunner` receives the
    /// `ProcessTable` it schedules from separately from the `&mut Runtime` it needs for
    /// BIF dispatch (it cannot lend the same table to both), so it constructs a context
    /// this way. The mailbox **scan state** is unaffected — it always lives in
    /// [`Runtime::mailboxes`](crate::runtime::Runtime), keyed by `pid`, because a
    /// suspended receive must resume against exactly the position it stopped at.
    ///
    /// `table` must contain `pid`; a receive-seam call for a pid it does not hold is the
    /// typed [`crate::error::no_mailbox`] capability gap, never a panic. A `send` through
    /// such a context delivers into `table` (the table `pid` belongs to), so a target pid
    /// living in a *different* table is reported as the table's typed not-found error.
    pub fn for_process(
        runtime: &'a mut Runtime,
        table: &'a mut ProcessTable,
        pid: ProcessId,
    ) -> RuntimeContext<'a> {
        RuntimeContext {
            runtime,
            output: None,
            mailbox_pid: pid,
            mailbox_table: Some(table),
            may_pump_scheduler: false,
            current_module: None,
            engines: None,
            gc_armed: false,
            dispatch_memo: DispatchMemo::default(),
        }
    }

    /// [`RuntimeContext::for_process`] plus the **engines** of the pass it runs in — the
    /// constructor the scheduler's runner uses (see the [`engines`](Self::engines) field).
    ///
    /// `engines` is the [`RunnerState`] the pass moved out of the runtime. The runner has
    /// taken the *executing* process's engine out of it (it drives that machine), so this
    /// view answers for every **other** live scheduled process.
    pub(crate) fn for_process_with_engines(
        runtime: &'a mut Runtime,
        table: &'a mut ProcessTable,
        pid: ProcessId,
        engines: &'a RunnerState,
    ) -> RuntimeContext<'a> {
        RuntimeContext {
            engines: Some(engines),
            ..RuntimeContext::for_process(runtime, table, pid)
        }
    }

    /// The engines of the scheduler pass this context is a slice of, if it is one.
    ///
    /// The value is `Copy`, so the caller gets the view without borrowing this context —
    /// which matters, because the consumer builds its reply terms on `&mut self` while it
    /// reads the machine (see [`crate::native::memory`]).
    pub(crate) fn engines(&self) -> Option<&'a RunnerState> {
        self.engines
    }

    /// The pid this context runs as (the mailbox owner and `erlang:self/0`).
    pub fn pid(&self) -> ProcessId {
        self.mailbox_pid
    }

    /// Whether this context may advance the scheduler (see the field docs).
    ///
    /// A `false` answer means this context is itself running inside a scheduler slice, so
    /// the evaluator's `receive` must report a capability gap rather than re-entering the
    /// scheduler.
    pub(crate) fn may_pump_scheduler(&self) -> bool {
        self.may_pump_scheduler
    }

    /// Arms the **GC latch** for the admission point the caller is about to make.
    ///
    /// The only caller is the slice's own
    /// [`BudgetContext::ensure_heap`](crate::runner), which arms it immediately before
    /// delegating the admission point to this context's
    /// [`ensure_heap`](ExecutionContext::ensure_heap); that method
    /// [`take`](std::mem::take)s the latch as its first statement, so it covers **exactly**
    /// that one admission point (see the [`gc_armed`](Self::gc_armed) field docs).
    pub(crate) fn arm_gc(&mut self) {
        self.gc_armed = true;
    }

    /// The process table a receive/send resolves against: the caller-supplied one when
    /// there is one, else the runtime's own.
    pub(crate) fn process_table_mut(&mut self) -> &mut ProcessTable {
        match self.mailbox_table {
            Some(ref mut table) => &mut **table,
            None => &mut self.runtime.processes,
        }
    }

    /// The read-only twin of [`RuntimeContext::process_table_mut`]: the table a pid is
    /// resolved against — the caller-supplied pass table when this context is a scheduler
    /// slice, else the runtime's own.
    ///
    /// A scheduler pass **moves** the process table out of the runtime
    /// ([`Runtime::take_processes`](crate::runtime::Runtime); the runtime keeps an empty
    /// placeholder), so a read that went to `runtime.processes()` unconditionally would see
    /// no process at all while code runs. The pass table is the one holding the live
    /// processes, and it is what `process_info/2`'s snapshot must read.
    pub(crate) fn process_table(&self) -> &ProcessTable {
        match &self.mailbox_table {
            Some(table) => &**table,
            None => self.runtime.processes(),
        }
    }

    /// Runs `f` with this context's process ([`Self::pid`]) marked **relocation-exempt**,
    /// restoring the previous mark **whatever `f` returns**.
    ///
    /// # What the exemption buys
    ///
    /// A delivery into an exempt process never relocates its heap
    /// ([`Process::set_relocation_exempt`](beam_process::Process::set_relocation_exempt)):
    /// an exhausted delivery returns the copier's **raw**
    /// [`Alloc(AllocKind::Message)`](BeamError::Alloc) refusal instead of collecting, and
    /// [`Self::deliver_message`] keys on exactly that to route the payload through the
    /// heap-fragment fallback ([`Self::send_fragmented_message`]) — so the message still
    /// arrives, with no word moved under the caller's feet.
    ///
    /// # Why the wrapped scopes need it
    ///
    /// Every wrapped scope runs code that holds `Eterm`s **outside** the root set a
    /// delivery-driven collection can publish: the `-eval` evaluator's `Env`, and the
    /// `x`/stack of a nested [`Machine`](beam_engine::Machine) built by
    /// [`Self::run_export_blocking`]/[`Self::run_label_blocking`]/[`Self::apply_fun`].
    /// [`Process::deliver`](beam_process::Process::deliver) collects the receiver's heap when
    /// a message does not fit, publishing only the process's own containers, so a term the
    /// caller copied out of an earlier message — an `io:scan_erl_exprs/4` result, say — would
    /// be recycled by the *next* message's delivery. An exempt process refuses that
    /// collection instead, which is what keeps the caller's handles valid.
    ///
    /// The scheduler slice's **running** window is deliberately **not** wrapped: the runner
    /// drives it with `run_export_outcome_with_tier`/`run_label_outcome_with_tier` directly,
    /// and its `BudgetContext` arms the per-call GC latch for the admission point that **is**
    /// the legitimate collection site
    /// ([`gc::ensure_heap`](crate::context::gc::ensure_heap)). Its **parked** window is marked
    /// by the runner instead: the Wait park arm sets the exemption
    /// ([`runner::run`](crate::runner)) and the resume clears it, so no delivery or
    /// `beam_process` signal relocates a waiting process's heap.
    ///
    /// # Nesting
    ///
    /// The previous mark is read back with
    /// [`ProcessTable::is_relocation_exempt`] and restored with
    /// [`ProcessTable::set_relocation_exempt`], so a nested scope leaves the outer scope's
    /// mark exactly as it found it — the `apply_fun` scope inside the
    /// `run_export_blocking` scope, say.
    ///
    /// # Errors
    ///
    /// `f`'s own error wins. The **restore** failure is surfaced only when `f` succeeded: a
    /// restore failure cannot un-run the closure, and the closure's diagnostic is the more
    /// useful one. A pid this context's table does not hold is not an error — there is no
    /// process to exempt, so `f` simply runs unguarded (whatever `f` does reports the miss).
    pub(crate) fn with_relocation_exempt<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let pid = self.mailbox_pid;
        self.with_pid_relocation_exempt(pid, f)
    }

    /// The **`pid`-addressed** form of [`Self::with_relocation_exempt`]: runs `f` with the
    /// process `pid` marked relocation-exempt, restoring the previous mark whatever `f`
    /// returns.
    ///
    /// [`Self::with_relocation_exempt`] is exactly `with_pid_relocation_exempt(self.mailbox_pid,
    /// f)` (the sending context's own process); the second caller is
    /// [`Self::deliver_message_with_off_heap_inner`], which marks the **receiver** when the
    /// unscanned-root valve says a delivery-driven collection of *its* heap would be unsound —
    /// a process that is not this context's own.
    ///
    /// A pid this context's table does not hold is not an error: there is no process to exempt,
    /// so `f` simply runs unguarded.
    pub(crate) fn with_pid_relocation_exempt<T>(
        &mut self,
        pid: ProcessId,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let previous = match self.process_table().is_relocation_exempt(pid) {
            Ok(previous) => Some(previous),
            // A pid with no process has no heap to move, so there is nothing to exempt.
            Err(BeamError::TermNotFound) => None,
            Err(other) => return Err(other),
        };
        if previous.is_some() {
            self.process_table_mut().set_relocation_exempt(pid, true)?;
        }
        let outcome = f(self);
        let Some(previous) = previous else {
            return outcome;
        };
        match self.process_table_mut().set_relocation_exempt(pid, previous) {
            Ok(()) => outcome,
            Err(restore) => match outcome {
                Ok(_) => Err(restore),
                Err(err) => Err(err),
            },
        }
    }

    /// Re-syncs a **resuming** machine from this context's process root set (the runner's
    /// resume seam).
    ///
    /// A parked machine can be relocated under it: while the machine is out of the runner, a
    /// collection that ran between slices — the pre-emption (`Yield`) window, which is never
    /// marked relocation-exempt — rewrites the process's `regs` in place, so the machine's own
    /// `x`/`stack`/`fr` point into the abandoned space. The process's root set is the truth a
    /// resume must run from, so this copies it back into the machine — all three containers
    /// ([`gc::scatter_roots_full`](crate::context::gc::scatter_roots_full), the FR file
    /// included). If their slice lengths no longer match, the process's collection is halted
    /// rather than run against inconsistent roots ([`crate::context::gc`] documents the
    /// hand-off).
    ///
    /// The machine's live **pending-exception reason** rides the process's
    /// [`extra_roots`](beam_process::Registers::extra_roots) (published by
    /// `runner::run::publish_parked_roots`), which such a collection has already relocated in
    /// place; this reads that relocated word back into
    /// [`Machine::pending_exception_mut`](beam_engine::Machine::pending_exception_mut) so the
    /// resumed machine points at the moved term, then **clears** the process's extra roots so
    /// no stale root outlives the resume. The reason is read out **before** the machine is
    /// borrowed by `with_roots` (the mutable accessor cannot be reached inside the closure).
    ///
    /// A `pid` with no process reconciles nothing: the runner only calls this for a parked pid
    /// whose process it owns.
    pub(crate) fn resync_parked_roots(&mut self, pid: ProcessId, machine: &mut Machine) {
        // The relocated reason, read out before `machine` is borrowed by `with_roots`.
        let relocated_reason = self
            .process_table()
            .get(pid)
            .and_then(|process| process.registers().extra_roots().first().copied());
        let reconciled = match self.process_table().get(pid) {
            Some(process) => machine.with_roots(RootSet::ALL_LIVE, |roots| {
                let mut roots = roots;
                gc::scatter_roots_full(process, &mut roots)
            }),
            None => true,
        };
        // Re-seat the relocated reason into the machine's own slot, so the resumed machine points
        // at the moved term rather than into the abandoned from-space.
        if let Some(reason) = relocated_reason {
            if let Some(slot) = machine.pending_exception_mut() {
                slot.1 = reason;
            }
        }
        if !reconciled {
            self.runtime.halt_process_gc(pid);
        }
        // Consume the published extra roots: the machine owns the relocated reason now, so a root
        // left behind would only keep an otherwise-dead term alive across the next collection.
        if let Some(process) = self.process_table_mut().get_mut(pid) {
            process.registers_mut().extra_roots_mut().clear();
        }
    }

    /// Whether this context owns the process table it resolves against.
    ///
    /// `true` for a context over the runtime's own table ([`RuntimeContext::new`],
    /// [`RuntimeContext::with_output`]) and `false` for one built by
    /// [`RuntimeContext::for_process`], whose table is owned by the scheduler pass. A native
    /// `erlang:spawn/*` reads it to pick **which** table the child goes in: outside a pass it
    /// appends to the runtime's own table (`Runtime::spawn_process`), and inside one it creates
    /// the child in the pass table and records it for the pass to register and enqueue, so the
    /// child is scheduled either way (see [`crate::native::process`]).
    pub(crate) fn owns_process_table(&self) -> bool {
        self.mailbox_table.is_none()
    }

    /// Heap words still available to the running code.
    ///
    /// In a **scheduled slice** whose process may be collected over this is the running
    /// process's own young free words — the heap the code really allocates from; every other
    /// context reports the runtime's unbounded, never-reclaimed arena (see
    /// [`crate::context::gc`]).
    pub fn heap_available(&self) -> usize {
        gc::heap_available(self)
    }

    /// The runtime's long-lived `-eval` heap.
    ///
    /// The evaluator builds a term here when the engine arena is not enough — a
    /// non-byte-aligned bitstring, which [`beam_codec::term::make_bitstring`] writes
    /// through a [`beam_alloc::Heap`]. That heap never moves its terms and
    /// Milestone-1 never resets it, so the term stays valid for the runtime's
    /// lifetime (see [`crate::runtime::Runtime::eval_heap`]).
    pub(crate) fn eval_heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.runtime.eval_heap
    }

    /// Records the module whose **code** this context is about to execute — the BIF call
    /// site, for the duration of one entry.
    ///
    /// [`RuntimeContext::run_export`] sets `current_module` itself for the single-pass path;
    /// a scheduled process runs its entry through the engine's blocking-aware
    /// [`Machine::run_export_outcome`](beam_engine::Machine::run_export_outcome) instead, so
    /// [`crate::runner`]'s process slice sets it here. The point is that a BIF which needs
    /// its call site — `erlang:load_nif/2` (whose owning module the oracle derives from the
    /// call instruction), the `records` source bridge — behaves identically whether the
    /// export ran in one pass or as a scheduled process.
    ///
    /// A module whose atom table cannot name itself records the empty name, exactly as
    /// [`RuntimeContext::run_export`]'s `unwrap_or_default` does — never a guessed one.
    pub(crate) fn enter_module(&mut self, module: &Module) {
        self.current_module = Some(
            module
                .atom_name(module.name)
                .unwrap_or_default()
                .to_string(),
        );
    }
}
