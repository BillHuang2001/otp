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

//! The concrete [`ExecutionContext`] glue: the engine's seam over the runtime core.
//!
//! The Stage-1 engine knows nothing about BIFs, heaps, processes or ETS — it calls
//! `call_bif`/`ensure_heap`/`alloc_words`/`apply_fun` on the trait and lets the host
//! decide (see `../beam_engine/src/context.rs`). This module is the host:
//! [`RuntimeContext`] borrows the [`Runtime`] (its BIF registry, the shared
//! [`AtomTable`](beam_term::AtomTable), the process heap, the calling [`ProcessId`], the
//! ETS [`TableRegistry`] and the output sink) and adapts them onto the engine.
//!
//! It is the **only** place the engine's seam meets the Stage-5 core; that keeps the
//! Stage-1/Stage-5 dependency DAG acyclic (`../CONTEXT.md`).
//!
//! # One context, three dispatch routes
//!
//! The engine routes `call_ext*` **and** `apply`/`apply_last` through
//! [`ExecutionContext::call_bif`] — unless the host can run the callee in-machine, which the
//! [`ExecutionContext::resolve_external_call`] seam reports — and `call_fun`/`call_fun2`
//! through the in-machine [`ExecutionContext::resolve_local_fun`] seam **first**, falling back
//! to the value-returning [`ExecutionContext::apply_fun`]. So [`RuntimeContext::call_bif`]
//! serves, in order:
//!
//! 1. the **runtime-native floor** ([`crate::native`]: `halt`, `display`,
//!    `io:put_chars`, `erlang:apply`, `init:get_argument`);
//! 2. the **`beam_bifs` registry** (the `bif.tab` surface);
//! 3. an **external module call**, loaded on demand from the code path through the
//!    [`CodeServer`](crate::CodeServer) and run with `self` as the context — the
//!    recursion that makes the runtime a real multi-module host.
//!
//! # The shared atom table
//!
//! A [`BifContext`] takes *one* [`AtomTable`](beam_term::AtomTable). The engine resolves
//! atoms — including the ones inside `Module` code — against the **loaded module's**
//! table (`beam_engine`'s `cmp(.., &module.atom_table)`). Because every module the
//! [`CodeServer`](crate::CodeServer) loads is interned into the runtime's **one shared**
//! table, handing the BIFs that same table is exactly right: an atom a BIF builds is an
//! index the engine can compare and the printer can name, for **every** loaded module.
//!
//! # Interning new atoms (the predicted-index commit)
//!
//! Some BIFs (`binary_to_term/1`, `list_to_atom/1`, …) must be able to add a **new** atom
//! to that same shared table, but the [`BifContext`] only borrows it *immutably* for index
//! resolution. Since this crate is `#![deny(unsafe_code)]`, the interner cannot hold a
//! second, mutable borrow of the one table. The [`AtomInterner`] hook solves this without
//! aliasing: [`PendingInterner`] records the names a BIF interns during a call and
//! **predicts** each index as `base + <names already recorded>` (interning is append-only
//! and the runtime is single-threaded, so the prediction is exact); after the BIF returns
//! — releasing the table borrow — [`RuntimeContext::call_registered`] commits the recorded
//! names into the shared table in the same order, so each lands at exactly the predicted
//! index. A name seen twice in one call is deduplicated and gets the same index.
//!
//! # The process-heap collector
//!
//! [`ensure_heap`](ExecutionContext::ensure_heap) and
//! [`alloc_words`](ExecutionContext::alloc_words) are served from the running **scheduled**
//! process's own young heap, collected at the heap-admission point — see [`gc`] for the
//! gate, the float guard and the root-set hand-off. Everything outside a scheduler slice
//! still runs on the runtime's pointer-stable
//! [`MinimalContext`](beam_engine::MinimalContext) arena, byte for byte as before.

mod bridges;
mod fun_identity;
pub(crate) mod gc;
mod memory_hook;
mod module_info;
mod persistent_term;
mod process_bridge;
mod runtime_hooks;
mod signal;
mod trace;#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use beam_abi::nif::loader::NifCallError;
use beam_bifs::{BifContext, OutputSink};
use beam_common::{AllocKind, BeamError, Result};
use beam_engine::context::RootSet;
use beam_engine::{terms, ExecutionContext, Machine, RunOutcome};
use beam_loader::{Atom, Label, Module};
use beam_message::{CopyContext, Message, OffHeapTable, Queue};
use beam_process::{ProcessId, ProcessTable};
use beam_term::Eterm;

use crate::error;
use crate::mailbox::MailboxState;
use crate::nif::{NifHostImpl, RuntimeNifServices};
use crate::port::RuntimePorts;
use crate::runner::RunnerState;
use crate::runtime::Runtime;

pub(crate) use self::bridges::{module_md5, PendingInterner};
pub(crate) use self::persistent_term::PersistentTermStore;
use self::bridges::{
    descriptor_of, DictBridge, FunBridge, NifBridge, NodeIdentity, RecordBridge, SendBridge,
    SharedSink,
};
use self::fun_identity::FunIdentityBridge;
use self::memory_hook::RuntimeMemory;
use self::module_info::ModuleInfoBridge;
use self::persistent_term::PersistentTermBridge;
use self::process_bridge::ProcessBridge;
use self::runtime_hooks::RuntimeHooksBridge;

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
const BLOCKING_PUMP_DEADLINE: Duration = Duration::from_secs(10);

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
fn pump_until_returned<F>(limit: Duration, mut outcome: RunOutcome, mut step: F) -> Result<Eterm>
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
    mailbox_pid: ProcessId,
    /// A **caller-supplied** process table to resolve that process against, if any.
    ///
    /// `None` means "use the runtime's own [`processes`](Runtime::processes)". The
    /// scheduler pass runs a process whose `Process` lives in a `ProcessTable` it owns
    /// separately from `&mut Runtime` (it cannot hand the same table to both), so the
    /// context must be able to resolve a mailbox against either source — see
    /// [`RuntimeContext::for_process`].
    mailbox_table: Option<&'a mut ProcessTable>,
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
    may_pump_scheduler: bool,
    /// The name of the module whose **code** is executing — the call site of a BIF.
    ///
    /// [`ExecutionContext::call_bif`] only supplies the *callee* module, but
    /// `erlang:load_nif/2` must know the module the **call site** lives in (the oracle derives
    /// it from the call instruction). [`RuntimeContext::run_export`] is the one place that
    /// knows it, so it sets this for the duration of the call and restores the previous value
    /// afterwards; [`RuntimeContext::call_registered`] forwards it to the [`BifContext`].
    current_module: Option<String>,
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
    engines: Option<&'a RunnerState>,
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
    gc_armed: bool,
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
    }    /// The process table a receive/send resolves against: the caller-supplied one when
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
    /// The scheduler slice is deliberately **not** wrapped: the runner drives it with
    /// `run_export_outcome_with_tier`/`run_label_outcome_with_tier` directly, publishes a
    /// parked machine's roots, and its `BudgetContext` arms the per-call GC latch for the
    /// admission point that **is** the legitimate collection site
    /// ([`gc::ensure_heap`](crate::context::gc::ensure_heap)).
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
    /// A parked machine can be relocated under it: while the process waits, a delivery-driven
    /// collection ([`Process::deliver`](beam_process::Process::deliver)'s exit cascade, which —
    /// unlike the in-slice admission point — is **not** gated by
    /// [`gc_process_enabled`](Runtime::gc_process_enabled)) rewrites the process's `regs` in
    /// place, so the machine's own `x`/`stack`/`fr` point into the abandoned space. The
    /// process's root set is the truth a resume must run from, so this copies it back into the
    /// machine — all three containers
    /// ([`gc::scatter_roots_full`](crate::context::gc::scatter_roots_full), the FR file
    /// included). If their slice lengths no longer match, the process's collection is halted
    /// rather than run against inconsistent roots ([`crate::context::gc`] documents the
    /// hand-off).
    ///
    /// A `pid` with no process reconciles nothing: the runner only calls this for a parked pid
    /// whose process it owns.
    pub(crate) fn resync_parked_roots(&mut self, pid: ProcessId, machine: &mut Machine) {
        let reconciled = match self.process_table().get(pid) {
            Some(process) => machine.with_roots(RootSet::ALL_LIVE, |roots| {
                let mut roots = roots;
                gc::scatter_roots_full(process, &mut roots)
            }),
            None => true,
        };
        if !reconciled {
            self.runtime.halt_process_gc(pid);
        }
    }

    /// Delivers `message` from `from` into the mailbox of the internal pid `to`, with **no**
    /// trace event emitted — the delivery half of [`ExecutionContext::send_message`], and the
    /// delivery a trace event itself uses (so emitting an event never emits an event).
    ///
    /// This is the **no-off-heap** form: it hands [`Self::deliver_message_with_off_heap`] an
    /// empty source table, so every runtime-internal delivery (`trace.rs`,
    /// `native/process/exit.rs`, [`Self::deliver_in_table`]) is byte-identical to before. The
    /// engine's `send` seam carries the sender machine's refc-binary handles and goes through
    /// the `_with_off_heap` form instead.
    ///
    /// A delivery pushes a woken recipient onto the table's wake list, so the wake survives for
    /// the scheduler pass; the outcome itself needs no action here.
    pub(crate) fn deliver_message(
        &mut self,
        to: ProcessId,
        from: Eterm,
        message: Eterm,
    ) -> Result<()> {
        let source = OffHeapTable::new();
        self.deliver_message_with_off_heap(to, from, message, &source)
    }

    /// [`Self::deliver_message`] for a sender that holds the term's **source off-heap table**
    /// — the payload handles the receiver-side copy resolves a refc binary by
    /// (`beam_message`'s `copy_refc_binary` looks a payload up by its data pointer through the
    /// copy's source table).
    ///
    /// `src` is the sender's own table: for the engine's `send` seam it is the in-flight
    /// machine's refc-binary handles, so `self() ! <<0:(1200)/integer>>` — an engine-built
    /// off-heap binary — is copied into the receiver instead of being refused with
    /// [`BeamError::BadArg`]. A sender with no off-heap payload (every runtime-internal
    /// delivery, and a plain on-heap message) passes an **empty** table, exactly as
    /// [`Self::deliver_message`] always did.
    ///
    /// # Ownership
    ///
    /// `src` is **borrowed for the whole call** — the [`CopyContext`] the receiver's copy runs
    /// under resolves payloads through it for the copy's duration — so the caller must keep it
    /// alive until this returns. The **destination** side is the receiver's own off-heap table
    /// (`ProcessTable::deliver` → `Process::deliver` → `beam_message::send` drains it into the
    /// queued [`Message`]), or the fragment's own table on the fallback, so — unlike the
    /// process-return path — **no** runtime-level destination table
    /// ([`Runtime::eval_off_heap`](crate::runtime::Runtime)) is needed. A handle must never be
    /// reconstructed from a raw payload data pointer.
    ///
    /// # Ordering (the oversized-message retry)
    ///
    /// The retry is triggered by the **raw** [`AllocKind::Message`] failure `deliver` returns:
    /// it means *the payload did not fit the receiver's heap* and is *not* fatal. The
    /// translation to `error:system_limit` is therefore applied only to what remains **after**
    /// the retry has run (the retry's own failure, and the non-`Message` arm) — translating the
    /// first result would turn the retry trigger into a hard error and break every oversized
    /// delivery.
    pub(crate) fn deliver_message_with_off_heap(
        &mut self,
        to: ProcessId,
        from: Eterm,
        message: Eterm,
        src: &OffHeapTable,
    ) -> Result<()> {
        match self.process_table_mut().deliver(to, from, message, src) {
            Err(BeamError::Alloc(failure)) if failure.kind == AllocKind::Message => self
                .send_fragmented_message(to, from, message, src)
                .map_err(error::enforce_term_heap_limit),
            other => other.map(|_| ()).map_err(error::enforce_term_heap_limit),
        }
    }

    /// [`Self::deliver_message`] for a caller that already holds the runtime and the
    /// destination's process table **separately** — the runtime's own delivery steps
    /// ([`Runtime::service_file_requests`](crate::runtime::Runtime),
    /// [`Runtime::deliver_port_messages`](crate::runtime::Runtime),
    /// [`Runtime::fire_due_timers`](crate::runtime::Runtime),
    /// [`Runtime::drain_ets_owner_exits`](crate::runtime::Runtime)), which run at a
    /// scheduler-pass boundary where the table has been moved out of the runtime.
    ///
    /// It carries the **same** delivery semantics — the plain receiver-heap copy, the
    /// heap-fragment fallback on the copier's raw [`AllocKind::Message`] refusal, and the
    /// `error:system_limit` translation — so every message a process receives arrives through
    /// one implementation. It matters most for a **relocation-exempt** receiver
    /// ([`Self::with_relocation_exempt`]): such a process refuses the collection an exhausted
    /// delivery would otherwise run, so without the fallback that refusal would fail the whole
    /// scheduler pass instead of delivering the message.
    ///
    /// The throwaway context exists only to reach that one method: `to` is the destination, so
    /// it addresses exactly the mailbox being delivered to and never pumps the scheduler
    /// ([`RuntimeContext::for_process`]).
    ///
    /// # Errors
    ///
    /// As [`Self::deliver_message`]: the table's typed `TermNotFound` for a pid no process
    /// exists for, and the fragment fallback's own failure translated to `error:system_limit`.
    pub(crate) fn deliver_in_table(
        runtime: &'a mut Runtime,
        table: &'a mut ProcessTable,
        to: ProcessId,
        from: Eterm,
        message: Eterm,
    ) -> Result<()> {
        RuntimeContext::for_process(runtime, table, to).deliver_message(to, from, message)
    }
    /// Delivers `message` into `to`'s mailbox through the **heap-fragment** model — the
    /// sender half of the C `erts_try_alloc_message_on_heap`/`new_message_buffer` path
    /// (`erl_message.c:622-691`), reached when the receiver's own heap cannot hold the
    /// payload (`../../erts/emulator/beam/erl_message.c`).
    ///
    /// The work is [`beam_message::send_fragmented`]`(from, message, heap, mailbox, ctx, 0)`
    /// driven straight into the receiver's own [`Process`](beam_process::Process) through
    /// [`heap_and_mailbox_mut`](beam_process::Process::heap_and_mailbox_mut) — the split
    /// borrow that hands out the receiver's young **heap** and its **mailbox** at the same
    /// time. The payload is copied into a fresh
    /// [`HeapFragment`](beam_message::HeapFragment) sized to hold all of it (the `0`
    /// capacity hint means "at least the whole payload"), the fragment is pushed onto a
    /// [`Fragments`](beam_message::Fragments) chain, and the enqueued
    /// [`Message`](beam_message::Message) owns that chain, so its payload has a stable home
    /// the receiver's heap is not involved in. The off-heap objects the payload references
    /// are refcounted into the **fragment's** table through the preflight's **source** table
    /// `src` — the sender's own handles
    /// ([`Self::deliver_message_with_off_heap`]'s) — so an engine-built off-heap binary in an
    /// oversized payload is resolved exactly as on the ordinary path.
    ///
    /// # The redundant preflight, and why it is harmless
    ///
    /// `send_fragmented` first retries the ordinary receiver-heap copy and falls back to a
    /// fragment only when *that* reports [`AllocKind::Message`]. This method is reached
    /// from exactly that failure, so the retry fails identically — and the retry is
    /// **side-effect free**: [`beam_message::copy_struct`] preflights the destination's
    /// capacity and refuses a graph it cannot hold *before* writing a word, so the
    /// receiver's heap is left byte for byte as it was (the composed body this replaced
    /// skipped the retry and went straight to a fragment).
    ///
    /// The recipient is woken exactly as the ordinary path wakes it, except that the wake
    /// cannot go through the process table's wake list (that list is filled by
    /// `ProcessTable::deliver`, which this path deliberately bypasses): it is recorded on
    /// [`Runtime::note_fragment_wake`] instead, and
    /// [`Runtime::scheduler_pass`](crate::runtime::Runtime) absorbs it into the real run
    /// queue at the same points it absorbs the table's own wake list.
    ///
    /// # Errors
    ///
    /// The failures are **raw**: the fragment's host-allocation failure and the copy
    /// engine's exhaustion ([`AllocKind::Message`]). The single caller
    /// ([`Self::deliver_message_with_off_heap`]) translates a genuine exhaustion to
    /// `error:system_limit` with [`error::enforce_term_heap_limit`]; that translation lives
    /// at the call site because the `Message` kind must stay recognizable as the retry
    /// trigger on the first `deliver` attempt.
    fn send_fragmented_message(
        &mut self,
        to: ProcessId,
        from: Eterm,
        message: Eterm,
        src: &OffHeapTable,
    ) -> Result<()> {
        // `src` is the sender's own source off-heap table (the engine seam's machine handles
        // on the `send` path), borrowed for the whole call: the preflight resolves a refc
        // binary's payload through it exactly as the ordinary delivery did.
        // A fresh destination off-heap table: the fragment path gives the payload to the
        // **fragment's** own table (see `beam_message::send_fragmented`), so this one is
        // only the preflight's — which cannot succeed here — and stays empty.
        let mut dst = OffHeapTable::new();
        let mut ctx = CopyContext::new(src, &mut dst);
        let process = self
            .process_table_mut()
            .get_mut(to)
            .ok_or(BeamError::TermNotFound)?;
        let (heap, mailbox) = process.heap_and_mailbox_mut();
        beam_message::send_fragmented(from, message, heap, mailbox, &mut ctx, 0)?;
        let woken = process.wake();
        if woken {
            self.runtime.note_fragment_wake(to);
        }
        Ok(())
    }

    /// The running process's mailbox **message queue** and **scan state**, borrowed
    /// together.
    ///
    /// The queue lives in the `Process` (in the caller-supplied table or the runtime's
    /// own); the scan state lives in [`Runtime::mailboxes`](crate::runtime::Runtime).
    /// They are disjoint, so both are handed out mutably at once. A pid with no process
    /// is the typed [`crate::error::no_mailbox`] capability gap.
    fn mailbox_parts(&mut self) -> Result<(&mut Queue, &mut MailboxState)> {
        let pid = self.mailbox_pid;
        let Runtime {
            processes,
            mailboxes,
            ..
        } = &mut *self.runtime;
        // `table` and `mailboxes`/`processes` are disjoint places, so the borrows below
        // do not conflict.
        let table: &mut ProcessTable = match self.mailbox_table {
            Some(ref mut table) => &mut **table,
            None => processes,
        };
        // A pid with no process is the capability gap *before* any scan state is
        // created for it, so a ghost pid never accumulates a stale entry.
        let process = table.get_mut(pid).ok_or_else(|| error::no_mailbox(pid))?;
        let state = mailboxes.entry(pid).or_default();
        Ok((process.mailbox_mut(), state))
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

    /// The payloads of the running process's mailbox, in FIFO order.
    ///
    /// This is the receive-seam **scan of the whole mailbox** the `-eval` evaluator needs
    /// (the engine's compiled `receive` walks the mailbox itself through
    /// `receive_next`/`receive_skip`, so it never needs this). The messages are *not*
    /// removed: [`RuntimeContext::remove_mailbox_message`] does that with the BEAM's
    /// `remove_message` ordering.
    ///
    /// A pid with no process is the typed [`crate::error::no_mailbox`] capability gap.
    pub(crate) fn mailbox_messages(&mut self) -> Result<Vec<Eterm>> {
        let (queue, _state) = self.mailbox_parts()?;
        Ok(queue.iter().map(Message::payload).collect())
    }

    /// Removes the message at `index` from the running process's mailbox and repositions
    /// the scan at the first message.
    ///
    /// `index` is a position in the FIFO order [`RuntimeContext::mailbox_messages`]
    /// reports; an index past the end removes nothing (the same safe no-op the queue's own
    /// `remove_at` implements for a past-the-end cursor).
    ///
    /// The removal goes through [`MailboxState::remove_at`], so a message whose payload
    /// lives in a heap fragment keeps it alive (`MailboxState::remove` explains why).
    pub(crate) fn remove_mailbox_message(&mut self, index: usize) -> Result<()> {
        let (queue, state) = self.mailbox_parts()?;
        let cursor = queue.cursor_at(index);
        state.remove_at(queue, cursor);
        state.reset(queue);
        Ok(())
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
    /// honest gap: no process could have changed the mailbox, so spinning further would only
    /// hang, never produce a value. A process that stays **runnable forever**, by contrast,
    /// reports progress on every round; [`pump_until_returned`] then gives up when its
    /// [`BLOCKING_PUMP_DEADLINE`] wall-clock cap elapses and reports a typed
    /// [`error::message`] diagnostic rather than spinning without end.
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
    fn run_export_blocking(
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
            let outcome = machine.run_export_outcome(module, function, arity, args, this)?;
            pump_until_returned(BLOCKING_PUMP_DEADLINE, outcome, |func, pc| {
                this.pump_once(&mut machine, module, func, pc)
            })
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
    /// honest gap: no process could have changed the mailbox, so spinning further would only
    /// hang, never produce a value. A process that stays **runnable forever**, by contrast,
    /// reports progress on every round; [`pump_until_returned`] then gives up when its
    /// [`BLOCKING_PUMP_DEADLINE`] wall-clock cap elapses and reports a typed
    /// [`error::message`] diagnostic rather than spinning without end.
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
    fn run_label_blocking(
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
            let outcome = machine.run_label_outcome(module, label, arity, args, this)?;
            pump_until_returned(BLOCKING_PUMP_DEADLINE, outcome, |func, pc| {
                this.pump_once(&mut machine, module, func, pc)
            })
        })
    }

    /// One pump-and-resume of a blocked cross-module run: advance the scheduler, then resume
    /// `machine` at the address its [`RunOutcome::Wait`] reported.
    ///
    /// `Ok(None)` means the mailbox cannot have changed since the last scan — either this
    /// context may not re-enter the scheduler ([`RuntimeContext::for_process`] is itself
    /// inside a slice) or the pump reported nothing ran — both the honest
    /// [`beam_engine::blocked`] gap (see [`RuntimeContext::run_export_blocking`]). A pump
    /// error (a process-table/port failure) is propagated unchanged.
    fn pump_once(
        &mut self,
        machine: &mut Machine,
        module: &Module,
        func: u32,
        pc: u32,
    ) -> Result<Option<RunOutcome>> {
        if !self.may_pump_scheduler() {
            return Ok(None);
        }
        let ran = self.runtime.pump_scheduler(RUN_EXPORT_PUMP_SLICES)?;
        if ran == 0 {
            return Ok(None);
        }
        Ok(Some(machine.resume_outcome(module, func, pc, self)?))
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
    /// Dispatches a `beam_bifs`-registered BIF over a [`BifContext`] built from the
    /// runtime's shared state and this context's output sink.
    ///
    /// The context is handed a [`PendingInterner`] so a BIF that builds a *new* atom
    /// (`binary_to_term/1`, `list_to_atom/1`, …) can intern it into the one shared table
    /// without aliasing it: the interner records the names, the BIF runs against the
    /// immutable table, and the names are committed (in order, at their predicted
    /// indices) once the BIF has returned and released the borrow — see the module docs.
    ///
    /// # Why every resource is a local
    ///
    /// The `fun`-taking `lists`/`maps` BIFs call back into the runtime through
    /// [`FunBridge`] (the [`FunCaller`] seam), which needs `&mut Runtime`. The
    /// [`BifContext`] therefore holds **no** borrow of the runtime: the registry, the
    /// atom table and the tables are cheap `Rc` clones (the atom handle dereferences to
    /// the shared table without borrowing the server), the heap comes from the BIF heap
    /// pool, and the output sink is a shared [`SharedSink`] handle cloned out of this
    /// context (see below). The only borrow of the runtime is the bridge's.
    ///
    /// A **nested** call (inside a fun a BIF applied) re-enters here through the bridge:
    /// it takes its own heap, shares the atom table through another `Rc` handle and shares
    /// the caller's sink through a [`SharedSink`] clone, and — because the outer call did
    /// **not** take the ETS cell (see [`Self::reentrancy_capable`]) — borrows the shared
    /// tables and reaches the outer tables' `ets:*` operations.
    ///
    /// # The process-dictionary seam
    ///
    /// The five process-dictionary BIFs (`erlang:put/2`, `get/1`, `erase/1`,
    /// `get_keys/0`, `get_keys/1`) need the **running process's** [`Dict`](beam_process::Dict)
    /// — a `&mut ProcessTable`, which for an `-eval` context is
    /// `&mut runtime.processes` and therefore cannot co-exist with [`FunBridge`]'s
    /// `&mut Runtime`. [`Self::uses_dict`] gates them: such a call installs a
    /// [`DictBridge`] (resolving the table the same way the mailbox/`self` seam does)
    /// **instead of** the fun-caller bridge. No dict BIF applies a fun argument, so nothing
    /// is lost (see [`DictBridge`]).
    fn call_registered(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        // Cheap handles that do NOT borrow the runtime: the runtime stays free for the
        // fun-caller bridge.
        let registry = Rc::clone(&self.runtime.registry);
        let atoms = self.runtime.code_server.atoms_handle();
        let tables_handle = Rc::clone(&self.runtime.tables);
        let ports_handle = Rc::clone(&self.runtime.ports);
        let mut heap = self.runtime.take_bif_heap()?;
        // `self_pid` is the pid this context runs as: `erlang:self/0` reports it, and a
        // `receive` in a fun a BIF applies resolves its mailbox against it.
        let self_pid = self.mailbox_pid;
        // `base` is the shared table's length before the call: the index the first new
        // name gets, because nothing else interns while this single-threaded call runs.
        let mut pending = PendingInterner::new(atoms.len());
        // Whether this is a process-dictionary BIF, which needs the running process's
        // `Dict` (a `&mut ProcessTable`) rather than a fun-caller bridge — the two borrows
        // cannot co-exist (see the method docs).
        let uses_dict = Self::uses_dict(module, function, arity);
        // Whether this is one of the eleven process/name/global-state BIFs, which need the
        // process-table/port-registry bridge (a `&mut ProcessTable` borrow, like the dict
        // arm — see [`ProcessBridge`]).
        let uses_process_hook = Self::uses_process_hook(module, function, arity);
        // Whether this is `erlang:load_nif/2`, which needs the NIF-loading bridge (also a
        // `&mut Runtime` borrow, and therefore a third mutually exclusive arm).
        let uses_nif_loader = Self::uses_nif_loader(module, function, arity);
        // Whether this is `records:get_definition/2`, which needs the record-source bridge
        // (a `&CodeServer` borrow, and therefore a fourth mutually exclusive arm).
        let uses_record_source = Self::uses_record_source(module, function, arity);
        // Whether this is `erlang:send/2`/`erlang:'!'/2`, which need the message-delivery
        // bridge (a fifth arm: it borrows the whole `Runtime` like the fun-caller bridge, and
        // the two cannot co-exist — see [`SendBridge`]).
        let uses_send_hook = Self::uses_send_hook(module, function, arity);
        // Whether this is `erlang:get_module_info/1,2`, which needs the module-info bridge
        // (a `&CodeServer` borrow, and therefore its own mutually exclusive arm).
        let uses_module_info = Self::uses_module_info(module, function, arity);
        // Whether this is one of the seven `persistent_term:*` BIFs, which need the
        // node-global store bridge (a cheap `Rc` clone: no runtime borrow at all).
        let uses_persistent_term = Self::uses_persistent_term(module, function, arity);
        // Whether this is one of the runtime-coupled BIF families (the timers, the loaded
        // export check, the link set) that only the running runtime can answer, and which
        // need the timer heap / reference space / process table (`&mut Runtime` plus a
        // `&mut ProcessTable`, like the dict arm — and therefore its own mutually exclusive
        // arm; see [`RuntimeHooksBridge`]).
        let uses_runtime_hooks = Self::uses_runtime_hooks(module, function, arity);
        // The local-node identity: **this runtime's own node** — the configured
        // `node@host` atom when `-name`/`-sname` made it a distributed node, else the fixed
        // `nonode@nohost` ([`Runtime::local_node`] is the single source of truth for both).
        // It is read from the runtime *before* the shared atom table is borrowed by the
        // context, and it is always `Some` for a live runtime: the atom was interned into
        // that very table at construction, so the hook can never answer "unknown" for a node
        // this runtime *is*. Both fields are `Copy`, so this hook needs no runtime borrow at
        // all and every arm below can install it (see [`NodeIdentity`]).
        let mut local_node = NodeIdentity {
            node: Some(self.runtime.local_node()),
        };
        // The caller's module name, cloned out *before* `self` is destructured: it outlives
        // the `BifContext` borrow below (the context stores `&str`, but a local `String` is
        // the owner), and it is forwarded to both the NIF bridge and the nested fun bridge.
        let caller_module = self.current_module.clone();
        // The local-fun identity: `term_to_binary/1,2` (and `external_size/1,2`, which sizes
        // the same image) of a compiled module's `make_fun3` closure needs the `NEW_FUN_EXT`
        // fields the term does not carry (module atom, module MD5, `old_index`/`old_uniq`,
        // creator pid), which only the *loaded module* knows. Like `local_node` this hook
        // needs **no** runtime borrow — it snapshots the caller module, its `.beam` path and
        // the node atom — but building that snapshot is not free (an `Arc` clone plus a
        // `PathBuf` allocation), so it is built **only** for the four BIFs that consult this
        // seam (see [`Self::uses_fun_identity`]). For every other registered call it stays
        // `None` and no hook is installed — indistinguishable from the hook itself, because
        // no other BIF ever calls `local_fun_identity`. With no calling module (the `-eval`
        // evaluator) the hook answers `Ok(None)`, preserving the codec's typed
        // `unsupported_local_fun` (see [`FunIdentityBridge`]).
        let mut fun_identity = if Self::uses_fun_identity(module, function, arity) {
            Some(FunIdentityBridge::new(
                &self.runtime.code_server,
                caller_module.as_deref(),
                local_node.node,
            ))
        } else {
            None
        };
        // The node's memory accounting behind `erlang:memory/0,1`: an owned snapshot holding
        // the shared atom table plus the figures reachable only *before* the call (the
        // process table, the code server, the ETS registry) — it needs **no** `&mut Runtime`
        // borrow and is built before the ETS `RefCell` is borrowed for the call, which is why
        // its ETS figure is captured (see [`RuntimeMemory`]). It is built **only** for
        // `erlang:memory/0,1` (see [`Self::uses_memory_hook`]): its `code` figure stats every
        // loaded module's `.beam` (one `stat` syscall per module, ~23 after boot), so building
        // it on *every* registered BIF call would add ~10 us to each arithmetic operation. No
        // other BIF consults the [`MemoryHook`] seam.
        let mut memory = if Self::uses_memory_hook(module, function, arity) {
            Some(RuntimeMemory::new(
                Rc::clone(&atoms),
                self.mailbox_table
                    .as_deref()
                    .unwrap_or(&self.runtime.processes),
                &self.runtime.code_server,
                &tables_handle,
            ))
        } else {
            None
        };
        let result = {
            let RuntimeContext {
                runtime,
                output,
                mailbox_pid,
                mailbox_table,
                may_pump_scheduler,
                current_module: _,
                engines,
                gc_armed: _,
            } = self;
            // Re-entry happens only when the BIF *applies a fun argument* (the fun-taking
            // `lists:*`/`maps:*` BIFs): the applied fun can call back into the runtime and
            // reach `ets:*`. Borrowing the shared `TableRegistry` for the whole call would
            // leave its `RefCell` taken, so the nested `ets:*` could not borrow it. When an
            // argument is a fun, therefore, do **not** install the tables — the cell stays
            // free for the nested call. Nothing is lost: a BIF that takes a fun scans it
            // with `lists`/`maps` logic and does not touch ETS itself (no M1 BIF both needs
            // ETS *and* applies a fun argument).
            let reentrant = Self::reentrancy_capable(args);
            let mut tables = if reentrant {
                None
            } else {
                // `try_borrow_mut` (never `borrow_mut`): an unexpected outer borrow must
                // degrade to "no ETS", never panic.
                tables_handle.try_borrow_mut().ok()
            };
            // The port provider is a cheap handle shared by both branches: it holds the
            // shared `Rc` port registry and a clone of the shared atom-table handle, so
            // `erlang:open_port/2` really starts an OS process and `erlang:port_info/1,2`
            // answers from the runtime's own port table (see `crate::port`).
            let mut ports = RuntimePorts::new(Rc::clone(&ports_handle), Rc::clone(&atoms), self_pid);
            if uses_dict {
                // The running process's directory: resolve the table the same way the
                // mailbox/`self` seam does — the caller-supplied table (a scheduler pass)
                // when there is one, else the runtime's own process table (a `-eval`
                // context). This borrow of `runtime` is the *only* one here, so it does not
                // clash with anything (the fun-caller bridge is not built).
                let table: &mut ProcessTable = match mailbox_table {
                    Some(ref mut table) => &mut **table,
                    None => &mut runtime.processes,
                };
                let mut dict = DictBridge {
                    table,
                    pid: self_pid,
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_dict_hook(&mut dict)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_process_hook {
                // The eleven process/name/global-state BIFs: the bridge borrows the process
                // table (the caller-supplied pass table when there is one, else the
                // runtime's own) *and* the runtime (for the `process_info` arena), exactly
                // like the dict arm, so it is its own mutually exclusive arm. None of them
                // applies a fun argument or touches the dictionary.
                let mut bridge = ProcessBridge::new(
                    runtime,
                    mailbox_table.as_deref_mut(),
                    Rc::clone(&ports_handle),
                    *engines,
                    self_pid,
                );
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_process_hook(&mut bridge)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_nif_loader {
                // `erlang:load_nif/2`: the NIF bridge needs `&mut Runtime` (to store the loaded
                // library in the runtime's NIF registry), so it is the only runtime borrow in
                // this arm — mutually exclusive with the dict/fun-caller arms. Its host shares
                // *this* call's `pending` interner, so a name the C `load` callback interns and
                // a name the BIF interns land in one ordered, exactly-predicted list.
                let mut bridge = NifBridge {
                    runtime,
                    atoms: Rc::clone(&atoms),
                    interner: pending.clone(),
                    self_pid: self_pid.to_eterm(),
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_nif_loader(&mut bridge)
                    .with_current_module(caller_module.as_deref().unwrap_or(""))
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_record_source {
                // `records:get_definition/2` answers from the *loaded module's* `Recs` chunk,
                // which lives in the code server's module map — a `&CodeServer` borrow, so it
                // is its own mutually exclusive arm (see [`RecordBridge`]). The BIF applies no
                // fun argument and touches no dictionary, so nothing is lost.
                let mut source = RecordBridge {
                    code_server: &runtime.code_server,
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_record_source(&mut source)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_send_hook {
                // `erlang:send/2`/`erlang:'!'/2` (`SendBridge`): the delivery bridge borrows the
                // whole `Runtime` (it delegates to the existing `RuntimeContext::send_message`
                // path, so nothing about delivery is re-implemented), making it mutually
                // exclusive with the fun-caller/dict/NIF/record arms. A `send` applies no fun
                // argument and touches no dictionary, so nothing is lost.
                let mut bridge = SendBridge {
                    runtime,
                    output: output.clone(),
                    mailbox_pid: *mailbox_pid,
                    mailbox_table: mailbox_table.as_deref_mut(),
                    may_pump_scheduler: *may_pump_scheduler,
                    current_module: caller_module,
                    engines: *engines,
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_sender(&mut bridge)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_module_info {
                // `erlang:get_module_info/1,2`: answers from the loaded module's IR — a
                // `&CodeServer` borrow, its own mutually exclusive arm. It applies no fun
                // argument.
                let mut source = ModuleInfoBridge {
                    code_server: &runtime.code_server,
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_module_info(&mut source)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_persistent_term {
                // The seven `persistent_term:*` BIFs: a cheap `Rc` clone of the
                // node-global store, plus a clone of the shared atom-table handle so the
                // store can compare keys/values **structurally** (`beam_term::eq`) — two
                // structurally equal boxed terms are distinct heap words. No runtime
                // borrow is held (it can coexist with everything).
                let mut store = PersistentTermBridge::new(
                    Rc::clone(&runtime.persistent_terms),
                    Rc::clone(&atoms),
                );
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_persistent_term(&mut store)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_runtime_hooks {
                // The runtime-coupled BIF families (`RuntimeHooksBridge`): the timer heap and
                // reference space (`&mut Runtime`) plus the caller's process table
                // (`&mut ProcessTable`) — the same pair the dict arm resolves, so it is its
                // own mutually exclusive arm. None of these keys applies a fun argument or
                // touches the dictionary, so the fun-caller/dict seams are never needed for
                // one, and every key this bridge does not serve answers the typed capability
                // gap the defaulted `RuntimeHooks` seam reports (workspace `CONTEXT.md` §3).
                let mut bridge = RuntimeHooksBridge {
                    runtime,
                    table: mailbox_table.as_deref_mut(),
                    self_pid,
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_runtime_hooks(&mut bridge)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else {
                // The bridge gets its own clone of the shared sink, so a `fun` a BIF applies
                // writes to the caller's sink (not stdout) — see `SharedSink`/`FunBridge`. It
                // inherits the caller's module name so a nested call still knows its call site.
                let mut bridge = FunBridge {
                    runtime,
                    output: output.clone(),
                    mailbox_pid: *mailbox_pid,
                    mailbox_table: mailbox_table.as_deref_mut(),
                    may_pump_scheduler: *may_pump_scheduler,
                    current_module: caller_module,
                    engines: *engines,
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_fun_caller(&mut bridge)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            }
        };
        // The BIF (and any NIF a bridge ran) is done and every borrow released, so commit the
        // names interned during the call: each lands at the index the interner predicted,
        // keeping the BIF's/NIF's atom terms (and every printer/comparison that resolves them)
        // consistent.
        //
        // The emptiness guard is load-bearing for performance, not semantics: an empty commit
        // is a no-op, but `atoms_mut()` goes through `Rc::make_mut`, and the port-registry /
        // persistent-term / NIF handles plus this call's own `atoms` binding keep the shared
        // table's strong count at 2 or more -- so touching it would deep-clone the whole
        // `AtomTable` on *every* registered BIF call. Only a call that actually interns a name
        // (`binary_to_term`/`list_to_atom`/`binary_to_atom`/`load_nif`, all rare) pays the
        // commit, and then it still lands the names at their predicted indices.
        if !pending.is_empty() {
            pending.commit(self.runtime.code_server.atoms_mut());
        }
        self.runtime.put_bif_heap(heap);
        // A registered BIF that ran its heap dry is the emulator's documented hard limit,
        // not a host-level allocation abort (see `error::enforce_term_heap_limit`).
        result.map_err(error::enforce_term_heap_limit)
    }

    /// Whether `(module, function, arity)` is one of the process-dictionary BIFs that must
    /// run against the calling process's [`Dict`](beam_process::Dict).
    ///
    /// These BIFs take a `&mut ProcessTable` (the running process), which cannot co-exist
    /// with [`FunBridge`]'s `&mut Runtime` borrow — so [`Self::call_registered`] installs a
    /// [`DictBridge`] for them **instead of** the fun-caller bridge. No dict BIF applies a
    /// fun argument, so the fun-caller seam is never needed for one.
    ///
    /// The set is [`beam_bifs::DICT_HOOK_BIFS`] — `beam_bifs`' own roster of the BIFs that
    /// read the [`DictHook`](beam_bifs::DictHook) seam — so this gate can never drift from
    /// what the registry actually routes through that seam.
    fn uses_dict(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_dict_hook(module, function, arity)
    }

    /// Whether `(module, function, arity)` is one of the process/name/global-state BIFs
    /// that run against the [`ProcessBridge`] (the process-table and port-registry seam),
    /// so the runtime-native floor no longer serves them and no BIF is duplicated.
    ///
    /// The set is [`beam_bifs::PROCESS_HOOK_BIFS`] — `beam_bifs`' own roster of the BIFs
    /// that read the [`ProcessHook`](beam_bifs::ProcessHook) seam.
    fn uses_process_hook(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_process_hook(module, function, arity)
    }

    /// Whether a call with `args` can re-enter the runtime by *applying a fun argument*.
    ///
    /// The `fun`-taking `lists`/`maps` BIFs (`lists:map/2`, `lists:foldl/3`, `maps:map/2`,
    /// …) take the fun they apply as a **direct argument** and call it back through
    /// [`FunBridge`]; a BIF with no fun argument never re-enters. [`Self::call_registered`]
    /// uses this to decide whether it may hold the shared ETS cell for the call (see the
    /// limitation documented there).
    fn reentrancy_capable(args: &[Eterm]) -> bool {
        args.iter().any(|arg| terms::is_fun(*arg))
    }

    /// Whether `(module, function, arity)` is `erlang:load_nif/2`, which must run against the
    /// [`NifBridge`] (the NIF-loading seam).
    ///
    /// The bridge needs `&mut Runtime` to store the loaded library, so it cannot co-exist with
    /// [`FunBridge`]'s `&mut Runtime` borrow or [`DictBridge`]'s `&mut ProcessTable` borrow —
    /// [`Self::call_registered`] installs exactly one of them. `load_nif/2` takes no fun
    /// argument and no dictionary, so nothing is lost.
    ///
    /// The set is [`beam_bifs::NIF_LOADER_BIFS`] — `beam_bifs`' own roster of the BIFs that
    /// read the [`NifLoader`](beam_bifs::NifLoader) seam.
    fn uses_nif_loader(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_nif_loader(module, function, arity)
    }

    /// Whether `(module, function, arity)` is `records:get_definition/2`, which must run
    /// against the [`RecordBridge`] (the record-source seam).
    ///
    /// The bridge borrows the code server's module map (`&Runtime`), so it cannot co-exist
    /// with [`FunBridge`]'s `&mut Runtime` or [`DictBridge`]'s `&mut ProcessTable` borrow —
    /// [`Self::call_registered`] installs exactly one of them. `get_definition/2` takes no
    /// fun argument and no dictionary, so nothing is lost.
    ///
    /// The set is [`beam_bifs::RECORD_SOURCE_BIFS`] — `beam_bifs`' own roster of the BIFs
    /// that read the [`RecordSource`](beam_bifs::RecordSource) seam.
    fn uses_record_source(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_record_source(module, function, arity)
    }

    /// Whether `(module, function, arity)` is a message-sending BIF that must run against the
    /// [`SendBridge`] (the delivery seam).
    ///
    /// `erlang:send/2` and `erlang:'!'/2` are the *same* `beam_bifs` implementation, which
    /// delivers through the [`SendHook`](beam_bifs::SendHook) (`beam_bifs/src/proc.rs`) — it
    /// cannot reach a mailbox itself (workspace `CONTEXT.md` §3: an uninstalled hook raises
    /// `unsupported_bif` rather than dropping the message). The bridge borrows the whole
    /// `Runtime` (it delegates to [`RuntimeContext::send_message`]), so it cannot co-exist with
    /// [`FunBridge`]'s borrow or [`DictBridge`]'s `&mut ProcessTable` borrow —
    /// [`Self::call_registered`] installs exactly one of the five. A `send` applies no fun
    /// argument and touches no dictionary, so nothing is lost.
    ///
    /// The compiled `Dest ! Msg` operator does **not** come through here: the BEAM compiler
    /// emits the `send` instruction, served by
    /// [`ExecutionContext::send_message`](beam_engine::ExecutionContext::send_message)
    /// directly. This predicate is for the `erlang:send/2`/`erlang:'!'/2` *BIF call* — which is
    /// what `erlang:send(Pid, Msg)` compiles to, and what the `-eval` evaluator dispatches for
    /// the same call.
    ///
    /// The set is [`beam_bifs::SEND_HOOK_BIFS`] — `beam_bifs`' own roster of the BIFs that
    /// read the [`SendHook`](beam_bifs::SendHook) seam.
    fn uses_send_hook(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_send_hook(module, function, arity)
    }

    /// Whether `(module, function, arity)` is `erlang:get_module_info/1,2`, which must run
    /// against the [`ModuleInfoBridge`] (the loaded-module-information seam).
    ///
    /// The bridge borrows the code server's module map (`&Runtime`), so it cannot co-exist
    /// with [`FunBridge`]'s `&mut Runtime` or [`SendBridge`]'s `&mut Runtime` borrow —
    /// [`Self::call_registered`] installs exactly one of them. `get_module_info/1,2` takes
    /// no fun argument and touches no dictionary, so nothing is lost.
    ///
    /// The set is [`beam_bifs::MODULE_INFO_BIFS`] — `beam_bifs`' own roster of the BIFs
    /// that read the [`ModuleInfo`](beam_bifs::ModuleInfo) seam.
    fn uses_module_info(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_module_info(module, function, arity)
    }

    /// Whether `(module, function, arity)` is one of the `persistent_term:*` BIFs,
    /// which must run against the [`PersistentTermBridge`] (the node-global store seam).
    ///
    /// The bridge holds a cheap clone of the runtime's shared store (`Rc<RefCell<…>>`), so
    /// it borrows **no** part of the runtime and can coexist with every other arm; it is an
    /// own `else if` arm only for symmetry and clarity. None of them takes a fun
    /// argument or a dictionary, so nothing is lost.
    ///
    /// The set is [`beam_bifs::PERSISTENT_TERM_BIFS`] — `beam_bifs`' own roster of the BIFs
    /// that read the [`PersistentTerm`](beam_bifs::PersistentTerm) seam.
    fn uses_persistent_term(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_persistent_term(module, function, arity)
    }

    /// Whether `(module, function, arity)` is one of the runtime-coupled BIFs the
    /// [`RuntimeHooksBridge`] serves: the timer family (`erlang:start_timer/3,4`,
    /// `erlang:send_after/4`, `erlang:cancel_timer/2`), the loaded-export check
    /// (`erlang:function_exported/3`), the link set (`erlang:link/1`, `erlang:unlink/1`),
    /// the dynamic-tracing tag pair (`erlang:dt_spread_tag/1`, `erlang:dt_restore_tag/1`)
    /// and the counters of `erlang:statistics/1`.
    ///
    /// These keys resolve through the **single** `beam_bifs` [`RuntimeHooks`] seam, whose
    /// default is the typed `unsupported_bif` capability gap. The bridge needs `&mut Runtime`
    /// (the timer heap, the reference space, the code server) **and** a `&mut ProcessTable`
    /// (the link set) — the same pair [`DictBridge`]/[`ProcessBridge`] resolve — so it cannot
    /// co-exist with the fun-caller bridge or the dict/process arms.
    ///
    /// # The gate is the whole seam roster, not just the served keys
    ///
    /// The set is [`beam_bifs::RUNTIME_HOOKS_BIFS`] — `beam_bifs`' own roster of **every**
    /// BIF that reads the [`RuntimeHooks`](beam_bifs::RuntimeHooks) seam (each registered
    /// through the `runtime_bif!` macro), which is far wider than the keys above. Gating on
    /// the whole roster is what keeps the two sides from drifting: a BIF that reads the
    /// seam but does not get the bridge installed can only ever answer the seam's typed
    /// `unsupported_bif` default, so a key `beam_bifs` starts routing through the seam must
    /// reach the bridge automatically. A key this bridge does not serve therefore answers
    /// the **same** typed capability gap the defaulted seam reports — only with the key
    /// named in the reason (the fallback arm of the bridge's own `RuntimeHooks::call`) —
    /// never a fabricated value.
    ///
    /// [`RuntimeHooks`]: beam_bifs::RuntimeHooks
    fn uses_runtime_hooks(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_runtime_hooks(module, function, arity)
    }

    /// Whether `(module, function, arity)` is one of the `erlang:memory/0,1` BIFs —
    /// the **only** consumers of the [`MemoryHook`](beam_bifs::MemoryHook) seam.
    ///
    /// These are not a bridge arm (the seam coexists with every arm) but a *build* gate:
    /// [`RuntimeMemory::new`] eagerly folds every loaded module's `.beam` image size, which
    /// is one `stat` syscall per loaded module, so building it on every registered call costs
    /// ~10 us per arithmetic BIF (see [`Self::call_registered`]). With no hook installed
    /// `beam_bifs`' `ctx.memory()` is the typed `unsupported_bif` capability gap, so the gate
    /// must cover **exactly** the keys that read the seam (workspace `CONTEXT.md` §3).
    ///
    /// The set is [`beam_bifs::MEMORY_HOOK_BIFS`] — `beam_bifs`' own roster of the BIFs that
    /// read that seam.
    fn uses_memory_hook(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_memory_hook(module, function, arity)
    }

    /// Whether `(module, function, arity)` is one of the BIFs that consult the
    /// [`LocalFunIdentity`](beam_bifs::LocalFunIdentity) seam — the **only** consumers of it.
    ///
    /// `beam_bifs`' `term_to_binary/1,2` and `external_size/1,2` both build the external-term
    /// image through `binconv::encode_body`, whose first read is
    /// `ctx.local_fun_identity(term)` (`beam_bifs/src/binconv.rs`); the `NEW_FUN_EXT`
    /// (`112`) identity of a compiled module's local (`make_fun3`) fun is the only thing that
    /// read produces. Building the bridge eagerly for *every* registered call copies the
    /// caller module's `Arc` and allocates its `.beam` path, so it is built only here — the
    /// hook's absence is otherwise indistinguishable, since no other BIF calls the accessor.
    ///
    /// The set is [`beam_bifs::LOCAL_FUN_IDENTITY_BIFS`] — `beam_bifs`' own roster of the
    /// BIFs that read that seam.
    fn uses_fun_identity(module: &str, function: &str, arity: u32) -> bool {
        beam_bifs::needs_local_fun_identity(module, function, arity)
    }

    /// Dispatches a **NIF** call without touching the runtime's ordinary dispatch.
    ///
    /// A perfect no-op on a miss (`Ok(None)`), so it is safe to try before *every* BIF: only a
    /// `(module, function, arity)` a loaded NIF library actually exports is intercepted, exactly
    /// as the BEAM's own NIF dispatch works (a NIF replaces the Erlang stub of the same name).
    ///
    /// The NIF runs against a **fresh** per-call [`NifHostImpl`] over a pooled BIF heap, with a
    /// fresh [`PendingInterner`] rooted at the shared table's current length. The term it
    /// returns lives on that pooled heap, which is pointer-stable and never reset (no GC in
    /// Milestone 1), so it stays valid after the heap is put back — the same convention
    /// [`Self::call_registered`] relies on.
    ///
    /// # Errors
    ///
    /// A NIF that records a pending exception (`enif_raise_exception`, `enif_make_badarg`)
    /// raises that reason as a live reason term ([`crate::error::nif_raised`]) — never swallowed
    /// and never replaced by a placeholder. A NIF that hands back a **non-term** (the ABI's
    /// `THE_NON_VALUE`, a bare header word) is that broken NIF's diagnostic, never a crash and
    /// never a fabricated value. An unexported `{name, arity}` (impossible here, the lookup
    /// already confirmed it) is the bare [`crate::error::undef`].
    fn try_call_nif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Option<Eterm>> {
        if !self.runtime.nifs.borrow().lookup(module, function, arity) {
            return Ok(None);
        }
        let atoms = self.runtime.code_server.atoms_handle();
        let interner = PendingInterner::new(atoms.len());
        let heap = self.runtime.take_bif_heap()?;
        let self_pid = self.mailbox_pid.to_eterm();
        let schedulers = crate::info::schedulers_count(self.runtime) as i32;
        // Clone the registry `Rc` so the call itself does not borrow `self.runtime` — the borrow
        // below is spent on `services`.
        let nifs = Rc::clone(&self.runtime.nifs);
        // The runtime services the host calls back into, over the same process table the rest of
        // the context resolves against (the slice's own table when there is one, else the
        // runtime's). These are **disjoint** field borrows of `self.runtime`, which is what lets
        // them co-exist with each other and with `self.mailbox_table`.
        let processes: &ProcessTable = match &self.mailbox_table {
            Some(table) => &**table,
            None => &self.runtime.processes,
        };
        let mut services = RuntimeNifServices {
            refs: &mut self.runtime.refs,
            references: &mut self.runtime.references,
            processes,
            ports: &self.runtime.ports,
        };
        let mut host = NifHostImpl::new(
            heap,
            Rc::clone(&atoms),
            interner.clone(),
            self_pid,
            schedulers,
            Some(&mut services),
        );
        // The registry is borrowed immutably only for the duration of the call, and released
        // before the interner is committed (which needs `&mut` on the code server).
        let outcome = {
            let reg = nifs.borrow();
            let mut env = beam_abi::NifEnv::new(&mut host);
            reg.call(module, function, arity, &mut env, args)
        };
        // Consuming the host releases its borrow of `services`, and dropping `services` releases
        // the runtime field borrows before the heap/interner work below reuses `self.runtime`.
        let heap = host.into_heap();
        drop(services);
        self.runtime.put_bif_heap(heap);
        // Same load-bearing emptiness guard as `call_registered`: `atoms_mut()` is
        // `Rc::make_mut`, and the live host/NIF-registry handles keep the shared table's
        // strong count above 1, so an unconditional commit would deep-clone the whole
        // `AtomTable` on every NIF call. Only commit when the NIF actually interned a name.
        if !interner.is_empty() {
            interner.commit(self.runtime.code_server.atoms_mut());
        }
        match outcome {
            // A NIF must hand back a **term** (or record an exception, which `NifLibrary::call`
            // turns into `Raised`). The ABI's `THE_NON_VALUE` (a bare header word) is not one: the
            // boxed-term predicates accept its primary tag, so letting it reach the engine would
            // dereference a null boxed pointer and abort. Report the broken NIF instead — a NIF
            // returning `enif_make_copy`'s "no value" word (or any other non-term) never crashes
            // the runtime and never becomes a fabricated value.
            Ok(term) if crate::nif::is_nif_value(term) => Ok(Some(term)),
            Ok(_) => Err(error::message("NIF returned a non-value term")),
            Err(NifCallError::Raised(reason)) => Err(error::nif_raised(reason)),
            // Defensive: `lookup` above already confirmed the export, so this cannot happen —
            // but a library raced out from under us would be `undef`, never a fabricated value.
            Err(NifCallError::UnknownFunction { .. }) => Err(error::undef()),
        }
    }
}

/// The launch target of a `.beam` **local** (`make_fun3`) closure.
///
/// A local closure's term carries only its `uniq` (per-module) and `index` (the module-level
/// lambda index); together they identify the [`FunDescriptor`] — hence the defining module
/// and its entry label — unambiguously across every module the code server has loaded. The
/// closure body is entered with the **free variables first**, followed by the call
/// arguments (a lambda's captured variables are its leading parameters).
///
/// This is the seam a **fun-bodied process**'s entry is resolved through: unlike
/// [`RuntimeContext::apply_fun`], which runs the closure on a fresh machine and therefore
/// folds a suspension into the typed [`blocked`](beam_engine::error::blocked) error, a
/// process entry has to enter the closure's own label on the **process's** machine so a
/// `receive` inside it can suspend the process and be resumed later
/// ([`crate::runner`]'s fun-entry arm).
///
/// [`FunDescriptor`]: beam_loader::FunDescriptor
pub(crate) struct LocalFunTarget {
    /// The fun's defining module (shared with the code server's cache).
    pub(crate) module: Arc<Module>,
    /// The lambda's entry label in that module.
    pub(crate) label: Label,
    /// The arity to enter the body with (`free.len() + args.len()`).
    pub(crate) arity: u32,
    /// The arguments to enter the body with: the fun's free variables, then the call
    /// arguments.
    pub(crate) args: Vec<Eterm>,
}

impl RuntimeContext<'_> {
    /// Resolves `fun` to the [`LocalFunTarget`] of a `.beam` **local** closure; `Ok(None)`
    /// when it has no local label to enter.
    ///
    /// `Ok(None)` covers the two fun kinds that are *not* a compiled local closure:
    ///
    /// * an **external** fun (`fun M:F/A`, the `EXPORT_EXT` encoding) — not a closure at
    ///   all, it carries no `uniq`/`index` descriptor and is applied as an ordinary dynamic
    ///   call through [`RuntimeContext::call_bif`];
    /// * the `-eval` evaluator's own closure (`uniq ==` [`crate::eval::EVAL_FUN_UNIQ`]),
    ///   whose body only the evaluator can run
    ///   ([`crate::eval::apply_eval_closure`]).
    ///
    /// Both are what [`RuntimeContext::apply_fun`] falls back to, so a caller that wants the
    /// blocking-aware local path (the runner's fun-entry arm) can keep the synchronous one
    /// for them.
    ///
    /// # Errors
    ///
    /// A non-fun is [`BeamError::BadArg`]; so is a local closure **no loaded module
    /// defines** (its `(uniq, index)` matches no `FunT` entry), and an `args`/free-variable
    /// count the descriptor's arity disagrees with is [`BeamError::BadArity`] — exactly what
    /// [`RuntimeContext::apply_fun`] reports for the same term, so nothing is fabricated.
    pub(crate) fn local_fun_target(
        &self,
        fun: Eterm,
        args: &[Eterm],
    ) -> Result<Option<LocalFunTarget>> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        if matches!(
            beam_codec::term::fun_kind_term(fun)?,
            beam_codec::FunKind::External
        ) {
            return Ok(None);
        }
        let uniq = terms::fun_uniq(fun)?;
        if uniq == crate::eval::EVAL_FUN_UNIQ {
            return Ok(None);
        }
        let index = terms::fun_index(fun)?;
        // Calling convention for a closure body (see `erts/emulator/beam/beam_common.c::call_fun`):
        // the call arguments occupy `x(0..arity)`, the captured free variables follow at
        // `x(arity..arity+free)`. The compiler emits exactly this layout — for
        // `fun({K,V}) -> {N,K,V} end` the body is `-f-fun-0-/2` and reads its *argument* from
        // `{x,0}` and its free variable `N` from `{x,1}`.
        let mut combined = args.to_vec();
        combined.extend_from_slice(terms::fun_free(fun)?);
        let found = self
            .runtime
            .code_server
            .modules()
            .find(|module| descriptor_of(module, uniq, index).is_some())
            .map(Arc::clone)
            .ok_or(BeamError::BadArg)?;
        let descriptor = descriptor_of(&found, uniq, index).ok_or(BeamError::BadArg)?;
        if combined.len() != descriptor.arity as usize {
            return Err(BeamError::BadArity);
        }
        Ok(Some(LocalFunTarget {
            module: found,
            label: descriptor.label,
            arity: descriptor.arity,
            args: combined,
        }))
    }

    /// Applies a fun that has **no local label entry** — the two kinds
    /// [`RuntimeContext::local_fun_target`] reports `Ok(None)` for.
    ///
    /// * An **external** fun (`fun m:f/A`, the `EXPORT_EXT` encoding) is not a closure at
    ///   all: it carries no `uniq`/`index` descriptor, so `terms::fun_uniq` rejects it with
    ///   `BadArg` and it must be recognised *before* that call. Its module/function/arity
    ///   are its whole identity, so applying it is an ordinary dynamic call through the
    ///   same dispatch every other call uses — the native floor, the registry, or a
    ///   load-on-demand module export. That is what makes `term_to_binary(fun
    ///   lists:reverse/1)` and `(fun lists:reverse/1)([1,2,3])` both work.
    /// * The `-eval` evaluator's own closure is applied through the evaluator's store
    ///   (`Runtime::eval_closures`): it may escape into a loaded module
    ///   (`lists:map(fun(X) -> X*X end, …)`) which calls back into it, and only the
    ///   evaluator can run its body.
    ///
    /// # Errors
    ///
    /// A non-fun is [`BeamError::BadArg`] (so a caller that skipped the classification gets
    /// the same answer [`RuntimeContext::local_fun_target`] would have given), plus whatever
    /// the dispatch or the evaluator reports.
    pub(crate) fn apply_non_local_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        if matches!(
            beam_codec::term::fun_kind_term(fun)?,
            beam_codec::FunKind::External
        ) {
            let (module, function, arity) = {
                let module_atom = beam_codec::term::fun_module(fun)?;
                let function_atom = beam_codec::term::fun_function(fun)?;
                let arity = beam_codec::term::fun_arity_term(fun)? as u32;
                // Resolve the atoms to names while the shared table is borrowed, then
                // release the borrow: `call_bif` needs `&mut self`.
                let atoms = self.runtime.code_server.atoms();
                let module = atoms.name(module_atom).ok_or(BeamError::BadArg)?;
                let function = atoms.name(function_atom).ok_or(BeamError::BadArg)?;
                (module.to_string(), function.to_string(), arity)
            };
            return self.call_bif(&module, &function, arity, args);
        }
        let index = terms::fun_index(fun)? as usize;
        crate::eval::apply_eval_closure(self, fun, index, args)
    }
}
impl ExecutionContext for RuntimeContext<'_> {
    /// The runtime's unified dispatch — see the module docs.
    ///
    /// 1. The runtime-native floor ([`RuntimeContext::try_native`]).
    /// 2. A registered `beam_bifs` BIF, with the runtime's **shared** atom table.
    /// 3. Otherwise an external module call: the module is loaded on demand from the
    ///    code path and run with `self` as the context. `erlang` is the BIF namespace,
    ///    never a loadable module, so an unknown `erlang:*` call is a typed diagnostic.
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        // A loaded NIF library wins over every other route: a NIF replaces the Erlang stub of
        // the same `{module, function, arity}`. On a miss this is a no-op (`Ok(None)`).
        if let Some(term) = self.try_call_nif(module, function, arity, args)? {
            return Ok(term);
        }
        if let Some(result) = self.try_native(module, function, arity, args) {
            // The floor builds its terms on the same pooled BIF heap, so an exhaustion
            // there is the same documented limit (see `error::enforce_term_heap_limit`).
            return result.map_err(error::enforce_term_heap_limit);
        }
        if self.runtime.registry.contains(module, function, arity) {
            // A table-creating `ets:new/2` is observed here so the runtime's owner index can
            // release the table when its creator dies (see `crate::runtime::ets_owner`). Only
            // a `named_table` is reachable through the registry's *public* surface, so only it
            // is indexed: the handle is resolved by the name registration the call newly made
            // (a `before`/`after` `lookup_name` delta distinguishes a real registration from a
            // pre-existing name or an unnamed table that merely carries the same name).
            if module == "ets" && function == "new" && arity == 2 {
                let name = args
                    .first()
                    .copied()
                    .filter(|term| beam_term::is_atom(*term))
                    .map(beam_term::atom_val);
                let before = name.and_then(|atom| self.runtime.tables.borrow().lookup_name(atom));
                let result = self.call_registered(module, function, arity, args);
                if let (Some(atom), Ok(_)) = (name, &result) {
                    let after = self.runtime.tables.borrow().lookup_name(atom);
                    if let Some(handle) = after.filter(|handle| Some(*handle) != before) {
                        // Indexing the table also takes ownership of its creation-time heir's
                        // gift term, which `beam_bifs` installed as a raw reference into the
                        // creating process's heap (see `runtime::ets_owner::own_heir_gift`).
                        self.runtime.adopt_created_table(self.mailbox_pid, handle, atom);
                    }
                }
                return result;
            }
            return self.call_registered(module, function, arity, args);
        }
        if module == "erlang" {
            // The **effective** gap set: the curated list intersected with what the live
            // registry does not own, so a freshly-registered BIF can never be shadowed (see
            // `error::is_known_unimplemented_bif`).
            if error::is_known_unimplemented_bif(&self.runtime.registry, function, arity) {
                return Err(error::known_unimplemented_bif(function, arity));
            }
            // A name the release does not have raises the bare `error:undef` the oracle does
            // (never a fabricated value and never a capability gap — see `error::undef`).
            return Err(error::undef());
        }
        // The code server adds the system library directories (stdlib, kernel) the first
        // time a module has to be resolved, so an external call reaches the real stdlib
        // without an explicit `-pa`. A miss here is not fatal: `CodeServer::load` below
        // reports it, naming every directory searched.
        let _ = self.runtime.find_module_file(module);
        let loaded = self.runtime.code_server.load(module)?;
        self.run_export(&loaded, function, arity, args)
    }

    /// The engine's **cross-module call seam**
    /// ([`ExecutionContext::resolve_external_call`]): run a `call_ext*` callee's export **in
    /// the running machine** instead of recursing through [`Self::call_bif`].
    ///
    /// The engine's `CallExt*` arms consult this **before** `call_import` (the route that
    /// reaches [`Self::call_bif`]): `Ok(Some((arc, label)))` makes the engine register `arc`
    /// as a module context and `Step::Enter` the callee's export entry with the caller's
    /// continuation stored in the live frame — so the callee's registers, Erlang stack and
    /// frames are the *same* root set the slice's own machine already hands
    /// [`ExecutionContext::ensure_heap`], and the slice's per-call GC latch covers every
    /// allocation the callee performs (see [`crate::context::gc`]). A recursion through
    /// `call_bif` instead builds a **second** [`Machine`](beam_engine::Machine) whose
    /// admission point is never armed, so every allocation past the process's young heap
    /// falls back to the runtime-wide arena — exactly the memory growth this removes.
    ///
    /// # What this must never do
    ///
    /// The seam is consulted for **every** `call_ext*`, including the names the
    /// runtime-native floor ([`crate::native`]) and the `beam_bifs` registry serve. Entering
    /// a discoverable `.beam` for one of those would run the module body *instead of* the
    /// host implementation — a wrong result — so the answer is deliberately conservative:
    ///
    /// * a name the floor serves ([`native::floor_shadows`]) → `Ok(None)`. The rule is
    ///   **key-granular**: a whole namespace in [`native::FLOOR_MODULES`] is refused, except
    ///   `io`, where the floor owns only `io:put_chars/1,2` — every other `io:*` is a real
    ///   `lib/stdlib/src/io.erl` body that must be entered in-machine so its blocking
    ///   `receive` can suspend the process;
    /// * a NIF-backed name ([`Runtime::nifs`]) — and, because the interception is module-wide
    ///   at dispatch, the *whole* module carrying a loaded NIF was excluded below —, a
    ///   registered `beam_bifs` name ([`Runtime::registry`]) or the `erlang` BIF namespace →
    ///   `Ok(None)`;
    /// * a module that is **not already loaded** is loaded on demand exactly as
    ///   [`Self::call_bif`] does (so the callee's `receive` suspends *this* process in the
    ///   first call too, not only the second); a load failure → `Ok(None)`, leaving
    ///   [`Self::call_bif`] to reproduce the identical `undef` diagnostic. A module loaded
    ///   that way whose own code reads the call site it runs at
    ///   ([`module_observes_its_call_site`]) → `Ok(None)` as well, so the on-demand route
    ///   never widens the caller-identity caveat documented above;
    /// * a callee whose code touches the machine's **float-register file** → `Ok(None)` (the
    ///   float guard — see [`crate::context::gc`]: the in-machine callee runs under the
    ///   caller's recorded module identity, so a collection over it could dangle a live float
    ///   register);
    /// * a function atom or export the callee does not have → `Ok(None)`, so the synchronous
    ///   path reports the oracle's `undef` byte for byte.
    ///
    /// Every `Ok(None)` leaves [`Self::call_bif`] — and therefore every observable result —
    /// unchanged: the seam only ever *substitutes a routing* for a call the synchronous path
    /// would run through the same code anyway.
    ///
    /// [`native::floor_shadows`]: crate::native::floor_shadows
    /// [`native::FLOOR_MODULES`]: crate::native::FLOOR_MODULES
    /// [`Runtime::nifs`]: crate::Runtime
    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        // The floor's namespaces: `try_native` serves them for modules that are *also*
        // loadable `.beam`s (`os`, `code`, `init`, `application`, `prim_file`,
        // `erts_internal`, `erlang`, `io`), so entering the module body would shadow the host
        // implementation. The exclusion is **key-granular** because one namespace is only
        // partly floor-served: the floor owns just `io:put_chars/1,2`, while every other
        // `io:*` (`io:getopts/1`, `io:setopts/2`, `io:request/2`, `io:scan_erl_form/4`, …) is
        // an ordinary `lib/stdlib/src/io.erl` body. Refusing those would route them onto the
        // synchronous non-pumping `call_bif` path, where a **scheduled** process's blocking
        // `receive` on the io-device reply folds into the typed `blocked()` gap (exit 3)
        // instead of suspending — the contract A.9 `compile:file/2` blocker; entering them
        // in-machine lets that `receive` suspend the process and be resumed when the device
        // replies. The key list is [`crate::native::floor_shadows`], whose `io` exception a
        // unit test below pins against `try_native`'s source so it can never drift out of the
        // host's dispatch.
        if crate::native::floor_shadows(module, function, arity) {
            return Ok(None);
        }
        // A module with a loaded NIF library is **never** entered here, for two reasons.
        //
        // * A NIF replaces the Erlang stub of the same `{module, function, arity}` at dispatch
        //   (the BEAM's order), so a NIF-backed name must keep reaching `try_call_nif` through
        //   `call_bif`. [`NifRegistry::lookup`] alone would answer that for the NIF's own
        //   names, but *not* for the module's ordinary Erlang functions.
        // * The interception is module-wide at **dispatch**, and the runtime's dispatch keys
        //   the module off [`Self::current_module`] — which an in-machine callee does **not**
        //   update (the engine's module-context switch never re-enters this context). Entering
        //   a NIF module's body would therefore run its calls under the *caller's* identity: a
        //   `erlang:load_nif/2` inside the body would target the caller module (the observed
        //   `bad_lib` on a reload), not itself.
        //
        // Excluding the whole module keeps such a call byte-for-byte on the synchronous
        // [`Self::call_bif`] path (which loads the module, sets the module identity and runs
        // it with a machine whose own admission point is unarmed) — exactly its behaviour
        // before this seam existed.
        //
        // [`NifRegistry::lookup`]: crate::nif::NifRegistry
        if self.runtime.nifs.borrow().is_loaded(module) {
            return Ok(None);
        }
        // A registered `beam_bifs` BIF (the `bif.tab` surface, including the stdlib fast
        // paths and `ets:*`) is the implementation `call_bif` serves; it is never a `.beam`
        // body here.
        if self.runtime.registry.contains(module, function, arity) {
            return Ok(None);
        }
        // Resolve the callee's module handle. A module that is **already cached** is entered
        // here exactly as before; an uncached one is loaded **on demand**, exactly as
        // [`Self::call_bif`] does, so the *first* call to a module runs in this machine too.
        // Without that, a `receive` inside the callee would fold into `error::blocked()` at the
        // call site — which is the A.9 boot blocker: `proc_lib:sync_start/2` waits in a module
        // its caller has not touched yet, so the first `call_ext` into it is the blocking one.
        //
        // The load is the same `find_module_file` + `CodeServer::load` pair [`Self::call_bif`]
        // runs, and it hands back the very same `Arc<Module>` the code server caches, so the
        // handle the engine keeps (and reuses for a suspended frame's return) is the one a later
        // call resolves to. A load failure returns `Ok(None)`: [`Self::call_bif`] then loads
        // again and reports the identical `undef` for a genuinely missing/malformed module, so
        // the seam never changes that diagnostic.
        let callee = match self.runtime.code_server.module_handle(module) {
            Some(callee) => callee,
            None => {
                let _ = self.runtime.find_module_file(module);
                let Ok(callee) = self.runtime.code_server.load(module) else {
                    return Ok(None);
                };
                // A module loaded **on demand here** is the only case this seam changes (it
                // used to take the synchronous path), and an in-machine callee runs under the
                // **caller's** recorded `current_module` (see the NIF exclusion above). Entering
                // a module whose own code *reads* that call site would therefore be new
                // exposure, so it keeps the synchronous [`Self::call_bif`] path — byte for byte
                // its behaviour before this seam loaded it. An already-cached module is
                // untouched by this check: entering it here is its pre-existing behaviour.
                if module_observes_its_call_site(&callee) {
                    return Ok(None);
                }
                callee
            }
        };
        // The **float guard** (see [`crate::context::gc`]): a module whose code touches the
        // machine's float-register file must never be collected over — `Machine::fr` holds
        // *terms* and is not part of the root set `ensure_heap` hands out, so a heap move with
        // a live float register would dangle it. An in-machine callee runs under the
        // **caller's** recorded `current_module` (the engine's module-context switch does not
        // re-enter this context), so entering a float-using callee here could collect under a
        // float-free caller's identity. Such a callee keeps the synchronous path instead,
        // which never collects over it — byte for byte its behaviour before this seam.
        if !self.runtime.module_is_float_free(module, &callee) {
            return Ok(None);
        }
        // Resolve the export in the **callee's own** atom table (`Module` code's atoms are
        // that table's, never the runtime's shared one) and hand back the export entry label
        // the synchronous `run_export` would enter. An unknown atom or export is the
        // synchronous path's `undef`.
        let Some(function_atom) = callee.atom_table.lookup(function) else {
            return Ok(None);
        };
        match callee.export(function_atom, arity) {
            Some(label) => Ok(Some((callee, label))),
            None => Ok(None),
        }
    }

    /// The **in-machine closure-apply seam** (`call_fun`/`call_fun2`): resolves a `.beam`
    /// **local** (`make_fun3`) closure to the module that defines it and the lambda's `FunT`
    /// entry label, so the engine enters the closure body **in this machine** — exactly as C's
    /// `call_fun` dispatches to the fun body's code in the same process
    /// (`../../erts/emulator/beam/beam_common.c:1604`).
    ///
    /// A `receive` inside the closure body therefore suspends **this** process through the
    /// ordinary [`beam_engine::RunOutcome::Wait`] path (and a raise lands in the caller's live
    /// `try`/`catch`), instead of the value-returning [`Self::apply_fun`] fallback running the
    /// body on a fresh nested machine — a suspension a **scheduled** slice's context
    /// ([`RuntimeContext::for_process`], `may_pump_scheduler() == false`) cannot resume, since a
    /// slice may never re-enter the scheduler.
    ///
    /// The resolution is the shared [`RuntimeContext::local_fun_target`] seam, so a closure
    /// resolves through exactly the lookup a fun-bodied process entry uses. The `module` it
    /// hands back is the **same** `Arc<Module>` the code server caches, which the engine
    /// requires because it dedups module contexts by `Arc::ptr_eq`.
    ///
    /// `Ok(None)` — the seam's "I cannot run this closure in your machine" — is passed through
    /// for the two fun kinds with no local label entry, exactly the cases
    /// [`RuntimeContext::local_fun_target`] reports `Ok(None)` for: an **external** fun
    /// (`fun M:F/A`, the `EXPORT_EXT` encoding) and the `-eval` evaluator's own closure. They
    /// keep the value-returning [`Self::apply_fun`] fallback, byte-identical to a host without
    /// the seam.
    ///
    /// The engine places the free variables itself (`x(arity..)`, C's `call_fun` order), so
    /// `local_fun_target` is consulted purely for resolution/validation — its combined
    /// `args ++ free` argument list is **not** used here.
    fn resolve_local_fun(
        &mut self,
        fun: Eterm,
        args: &[Eterm],
    ) -> Result<Option<(Arc<Module>, Label)>> {
        Ok(self
            .local_fun_target(fun, args)?
            .map(|target| (target.module, target.label)))
    }

    /// Ensures `heap_need` words are available for the running code, collecting the running
    /// process's own young heap when it cannot serve them (see [`crate::context::gc`]).
    ///
    /// The [`gc_armed`](Self::gc_armed) latch is [`take`](std::mem::take)n as the **first
    /// statement**, so only the admission point the slice's own
    /// [`BudgetContext`](crate::runner) armed can collect: a nested machine built on this
    /// same context (an external module call, a fun a BIF applied, the evaluator) finds the
    /// latch clear and can never be collected over.
    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        let armed = std::mem::take(&mut self.gc_armed);
        gc::ensure_heap(self, heap_need, live, x_regs, stack, armed)
    }

    /// Ensures `heap_need` words are available with the machine's **whole live root set**
    /// handed in, collecting over all of it (see [`crate::context::gc`]).
    ///
    /// This is the primary admission point: `test_heap`/`allocate_heap` reach it through
    /// [`Machine::ensure_heap`](beam_engine::Machine), so a collection here sees **every** x
    /// register, the whole Erlang stack **and the whole FR file** — not just the x prefix the
    /// engine declared live. The [`gc_armed`](Self::gc_armed) latch is
    /// [`take`](std::mem::take)n exactly as in [`ensure_heap`](Self::ensure_heap), so only the
    /// admission point the slice's own [`BudgetContext`](crate::runner) armed can collect.
    fn ensure_heap_full(&mut self, heap_need: u32, roots: RootSet<'_>) -> Result<()> {
        let armed = std::mem::take(&mut self.gc_armed);
        gc::ensure_heap_full(self, heap_need, roots, armed)
    }

    /// Heap words still available (see [`RuntimeContext::heap_available`]).
    fn heap_available(&self) -> usize {
        gc::heap_available(self)
    }

    /// Allocates `n` raw heap words for the running code: the running process's own young
    /// heap in a scheduled slice, the runtime's pointer-stable arena everywhere else (see
    /// [`crate::context::gc`]).
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        gc::alloc_words(self, n)
    }

    /// Allocates `n` raw heap words with the machine's **whole live root set** handed in, so a
    /// slice whose young heap cannot serve the request collects over those roots and retries
    /// (see [`crate::context::gc`]).
    ///
    /// The [`gc_armed`](Self::gc_armed) latch is [`take`](std::mem::take)n here too, so a
    /// nested machine ([`Self::run_export`], a fun a BIF applied, the evaluator) behaves
    /// exactly like [`alloc_words`](Self::alloc_words) and never collects.
    fn alloc_words_rooted(&mut self, n: usize, roots: RootSet<'_>) -> Result<*mut Eterm> {
        let armed = std::mem::take(&mut self.gc_armed);
        gc::alloc_words_rooted(self, n, roots, armed)
    }

    /// Applies a fun/closure value.
    ///
    /// A `.beam` **local** (`make_fun3`) closure is resolved to its
    /// [`LocalFunTarget`] — the module, the lambda's entry label and the combined
    /// arguments — and entered there on a **fresh** machine through the
    /// blocking-aware [`RuntimeContext::run_label_blocking`], so a `receive` inside
    /// the closure body suspends the process and is resumed by the scheduler
    /// instead of folding into the typed [`beam_engine::blocked`] gap. The
    /// resolution is the shared [`RuntimeContext::local_fun_target`] seam, so a
    /// fun-bodied *process* resolves its entry through exactly the same lookup
    /// ([`crate::runner`]).
    ///
    /// The two fun kinds with no local label entry keep their own synchronous apply:
    ///
    /// * an **external** fun (`fun M:F/A`, the `EXPORT_EXT` encoding) is not a closure
    ///   at all — its module/function/arity are its whole identity, so applying it is an
    ///   ordinary dynamic call through [`RuntimeContext::call_bif`], which is what makes
    ///   `term_to_binary(fun lists:reverse/1)` and `(fun lists:reverse/1)([1,2,3])` both
    ///   work;
    /// * a fun built by the `-eval` evaluator is applied through the evaluator's own
    ///   store (`Runtime::eval_closures`): the fun may escape into a loaded module
    ///   (`lists:map(fun(X) -> X*X end, …)`) which calls back into it, and the closure
    ///   body can only be run by the evaluator.
    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        // A local closure enters a **fresh** `Machine` (below, through `run_label_blocking`),
        // and the `-eval`-closure arm runs the evaluator's own environment; neither set of
        // terms is in the process root set, so the process is relocation-exempt for the whole
        // application (see [`Self::with_relocation_exempt`]).
        self.with_relocation_exempt(|this| match this.local_fun_target(fun, args)? {
            Some(LocalFunTarget {
                module,
                label,
                arity,
                args,
            }) => this.run_label_blocking(&module, label, arity, &args),
            None => this.apply_non_local_fun(fun, args),
        })
    }
    /// `send`: deliver `message` to the pid term `to`.
    ///
    /// `to` is resolved with [`ProcessId::from_eterm`] (a non-pid is [`BeamError::BadArg`])
    /// and the message is copied into the recipient's heap and enqueued by
    /// [`ProcessTable::deliver`] (an unknown pid is the table's typed
    /// `TermNotFound`). The sender is this context's pid, so `self() ! M` reaches the very
    /// mailbox the following `receive` scans.
    ///
    /// This is the **no-off-heap** form: it delegates to
    /// [`Self::send_message_with_off_heap`] with an empty handle slice, so the trace events
    /// and the delivery live in one place. A sender whose message may hold an engine-built
    /// off-heap binary goes through the `_with_off_heap` form.
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        self.send_message_with_off_heap(to, message, &[])
    }

    /// `send` with the **source off-heap table** the receiver-side copy needs.
    ///
    /// `src_binaries` are the payload handles of the machine that built `message` — the
    /// in-flight `Machine::refc_binaries()` slice the engine's `send` arm hands over
    /// ([`ExecutionContext::send_message_with_off_heap`]). A `bs_init_writable` /
    /// `bs_create_bin` result over `beam_term::refc::ERL_ONHEAP_BINARY_LIMIT` bytes is stored
    /// **off** the process heap, so the copy into the recipient
    /// ([`ProcessTable::deliver`]) resolves a refc binary by its data pointer through this
    /// table; without it the copy would refuse the whole message with
    /// [`BeamError::BadArg`]. An empty slice (every runtime-internal send, and any message
    /// with no off-heap payload) is byte-identical to the no-off-heap path.
    ///
    /// The source table is a **local that outlives the whole delivery call** — the copy
    /// borrows it for the copy's duration — and `deliver_message_with_off_heap` reads it. The
    /// destination is the receiver's own off-heap table, so no runtime-level table is needed
    /// (see [`Self::deliver_message_with_off_heap`]).
    ///
    /// A completed send also produces **events** (contract A.8, item 1): the ordinary
    /// `{trace, Sender, send, Msg, To}` — when the *sending* process is `[send]`-traced — and
    /// the sequential-trace `{seq_trace, Label, {send, …}}` when its token has the `send`
    /// flag. Both are delivered to the relevant tracer as messages of their own, after the
    /// payload ([`Self::trace_send_events`]). A third event follows when the **receiver** is
    /// `['receive']`-traced: `{trace, Receiver, 'receive', Msg}`, emitted here (never from the
    /// internal delivery bypass, which would recurse) by [`Self::trace_receive_events`].
    fn send_message_with_off_heap(
        &mut self,
        to: Eterm,
        message: Eterm,
        src_binaries: &[beam_alloc::RefcBinary],
    ) -> Result<()> {
        let target = ProcessId::from_eterm(to)?;
        let from = self.mailbox_pid.to_eterm();
        if src_binaries.is_empty() {
            self.deliver_message(target, from, message)?;
        } else {
            // The sender's source off-heap table, built from the machine's handles (each
            // `clone` bumps the `RefcBinary` refcount). It is a local that must outlive the
            // whole delivery: the receiver-side copy borrows it (see
            // `deliver_message_with_off_heap`).
            let mut source = OffHeapTable::new();
            for handle in src_binaries {
                source.push_binary(handle.clone());
            }
            self.deliver_message_with_off_heap(target, from, message, &source)?;
        }
        self.trace_send_events(to, message)?;
        // The delivered message is also a `receive` event for the **receiver** — gated on the
        // receiver's own `receive` flag, emitted after the `send` event, and emitted here (the
        // public send seam) rather than in `deliver_message`, which is the bypass every trace
        // event itself is delivered through: emitting there would recurse forever.
        self.trace_receive_events(target, message)
    }

    /// `loop_rec`: the candidate at the current mailbox scan position, `Ok(None)` when
    /// the scan/mailbox is exhausted.
    ///
    /// **Peeks**: the scan position does not move (the following `loop_rec_end` advances
    /// it). Never `unsupported` while the running process exists — an empty mailbox is
    /// simply `Ok(None)`, which makes the compiled code jump to the `wait` that blocks.
    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        let (queue, state) = self.mailbox_parts()?;
        Ok(state.next(queue))
    }

    /// `loop_rec_end`: advance the scan past the current candidate.
    fn receive_skip(&mut self) -> Result<()> {
        let (queue, state) = self.mailbox_parts()?;
        state.skip(queue);
        Ok(())
    }

    /// `remove_message`: unlink the matched candidate and reposition the scan at the
    /// first message.
    fn receive_remove(&mut self) -> Result<()> {
        let (queue, state) = self.mailbox_parts()?;
        state.remove(queue);
        Ok(())
    }

    /// `timeout`: reset the scan position to the first message.
    fn receive_reset(&mut self) -> Result<()> {
        let (queue, state) = self.mailbox_parts()?;
        state.reset(queue);
        Ok(())
    }

    /// `recv_marker_reserve`: an opaque marker term for the current scan position.
    ///
    /// On an **empty** queue there is no position to record, so the C `am_undefined` is
    /// returned instead — interned into the one shared atom table so the term is a real,
    /// nameable atom (and, being non-small, makes the following `recv_marker_bind` a
    /// no-op). Otherwise a fresh small-integer marker is returned.
    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        let undefined = self.runtime.intern("undefined");
        let (queue, state) = self.mailbox_parts()?;
        Ok(state.reserve(queue, undefined))
    }

    /// `recv_marker_bind`: bind the opaque `marker` to the (internal) reference.
    ///
    /// A non-small marker (the empty-queue `undefined`) is a silent no-op.
    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        let (_queue, state) = self.mailbox_parts()?;
        state.bind(marker, reference);
        Ok(())
    }

    /// `recv_marker_use`: set the scan position to the position `reference`'s marker
    /// recorded (a no-op for an unknown reference).
    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        let (queue, state) = self.mailbox_parts()?;
        state.use_reference(queue, reference);
        Ok(())
    }

    /// `recv_marker_clear`: release `reference`'s marker (a no-op for an unknown one).
    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        let (_queue, state) = self.mailbox_parts()?;
        state.clear(reference);
        Ok(())
    }

    /// Whether the running process requests the `call`/`return_from`/`return_to` execution
    /// trace family (contract A.8).
    ///
    /// The engine asks this **once per run** and latches it into `Machine::trace_on`, so the
    /// body ([`RuntimeContext::execution_trace_enabled`], in the sibling `context/trace.rs`)
    /// is a single hash lookup and an untraced run pays one boolean.
    fn trace_enabled(&self) -> bool {
        self.execution_trace_enabled()
    }

    /// Report one execution-trace event (contract A.8).
    ///
    /// The body ([`RuntimeContext::execution_trace_event`], in the sibling `context/trace.rs`)
    /// decides matching against the installed `erlang:trace_pattern/3` table and delivers the
    /// `{trace, …}` message to the tracee's tracer. It is **infallible**: the engine ignores
    /// this method's result, so an unresolvable name or an undeliverable event drops the
    /// event rather than failing the traced program.
    fn trace_event(&mut self, event: beam_engine::TraceEvent<'_>) {
        self.execution_trace_event(event);
    }
}

/// Whether `module`'s **own code** can read the call site it runs at, i.e. whether it imports a
/// BIF whose answer depends on [`RuntimeContext::current_module`].
///
/// The engine's cross-module call seam ([`ExecutionContext::resolve_external_call`]) runs a
/// callee inside the **caller's** machine, and the engine's module-context switch never re-enters
/// this context, so a callee keeps the caller's `current_module`. The two imported BIFs that
/// *read* that identity are:
///
/// * `erlang:load_nif/2` — the module a NIF library is registered under, which the library's own
///   `ErlNifEntry.name` is validated against: a stale identity is the oracle's `bad_lib` (and it
///   is why a module carrying a *loaded* NIF is excluded from the seam altogether);
/// * `erlang:term_to_binary/1,2` and `erlang:external_size/1,2` — the `NEW_FUN_EXT` identity
///   (module atom, module MD5, `old_index`/`old_uniq`) of a **local** fun argument, which
///   [`FunIdentityBridge`] derives from the calling module: a stale identity would name the wrong
///   module.
///
/// The other `current_module` readers cannot be made wrong by a callee's stale identity:
/// `records:get_definition/2` resolves its module from its **argument** ([`RecordBridge`] holds no
/// call-site state at all), and the GC float guard ([`crate::context::gc`]) / the trace
/// global-pattern approximation only use it conservatively.
///
/// The scan is over the module's `ImpT` table ([`Module::imports`]), which is where a compiled
/// `M:F/A` call is recorded. A call reached only through `apply/3` is not covered — that is the
/// documented residual, and it is no wider than the exposure the seam already had for a module
/// that was cached before the call.
///
/// [`ExecutionContext::resolve_external_call`]: beam_engine::ExecutionContext::resolve_external_call
/// [`Module::imports`]: beam_loader::Module::imports
pub(crate) fn module_observes_its_call_site(module: &Module) -> bool {
    module.imports.iter().any(|import| {
        module.atom_name(import.module) == Some("erlang")
            && match module.atom_name(import.function) {
                Some("load_nif") => import.arity == 2,
                Some("term_to_binary") | Some("external_size") => {
                    import.arity == 1 || import.arity == 2
                }
                _ => false,
            }
    })
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
    use super::*;
    use crate::context::test_support::{compile_fixture, find_on_path, resolve_erlc, skip_loudly};
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
