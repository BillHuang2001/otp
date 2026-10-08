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
//! [`ExecutionContext::call_bif`], and `call_fun`/`call_fun2` through
//! [`ExecutionContext::apply_fun`]. So [`RuntimeContext::call_bif`] serves, in order:
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
//! # No collector yet
//!
//! [`ensure_heap`](ExecutionContext::ensure_heap) and
//! [`alloc_words`](ExecutionContext::alloc_words) are served by the runtime's
//! pointer-stable [`MinimalContext`](beam_engine::MinimalContext) arena. That arena
//! treats the heap as unbounded, so `ensure_heap` never reports a short heap — there is
//! no GC in the engine yet (`../beam_engine` Known Issues). The production collector is
//! Stage 4 (`beam_gc`), which will run *here* and may move terms.

mod bridges;
mod fun_identity;
mod memory_hook;
mod module_info;
mod persistent_term;
mod process_bridge;
mod runtime_hooks;
mod trace;#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use std::rc::Rc;
use std::sync::Arc;

use beam_abi::nif::loader::NifCallError;
use beam_bifs::{BifContext, OutputSink};
use beam_common::{AllocKind, BeamError, Result};
use beam_engine::{terms, ExecutionContext, Machine};
use beam_loader::Module;
use beam_message::{Fragments, HeapFragment, Message, OffHeapTable, Queue};
use beam_process::{ProcessId, ProcessTable};
use beam_term::Eterm;

use crate::error;
use crate::mailbox::MailboxState;
use crate::nif::NifHostImpl;
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

    /// Delivers `message` from `from` into the mailbox of the internal pid `to`, with **no**
    /// trace event emitted — the delivery half of [`ExecutionContext::send_message`], and the
    /// delivery a trace event itself uses (so emitting an event never emits an event).
    ///
    /// `deliver_simple` pushes a woken recipient onto the table's wake list, so the wake
    /// survives for the scheduler pass; the outcome itself needs no action here. A payload the
    /// recipient's heap cannot hold is retried through the heap-fragment model
    /// ([`Self::send_fragmented_message`]) exactly as
    /// [`send_message`](ExecutionContext::send_message) documents; every other failure — the
    /// table's typed not-found for a pid no process exists for, and a genuine exhaustion — is
    /// propagated, the latter translated to the emulator's documented
    /// `error:system_limit` ([`error::enforce_term_heap_limit`]).
    ///
    /// # Ordering (the oversized-message retry)
    ///
    /// The retry is triggered by the **raw** [`AllocKind::Message`] failure
    /// `deliver_simple` returns: it means *the payload did not fit the receiver's heap* and
    /// is *not* fatal. The translation to `error:system_limit` is therefore applied only to
    /// what remains **after** the retry has run (the retry's own failure, and the
    /// non-`Message` arm) — translating the first result would turn the retry trigger into a
    /// hard error and break every oversized delivery.
    pub(crate) fn deliver_message(
        &mut self,
        to: ProcessId,
        from: Eterm,
        message: Eterm,
    ) -> Result<()> {
        match self.process_table_mut().deliver_simple(to, from, message) {
            Err(BeamError::Alloc(failure)) if failure.kind == AllocKind::Message => self
                .send_fragmented_message(to, from, message)
                .map_err(error::enforce_term_heap_limit),
            other => other.map(|_| ()).map_err(error::enforce_term_heap_limit),
        }
    }
    /// The running process's mailbox **message queue** and **scan state**, borrowed
    /// together.
    ///
    /// The queue lives in the `Process` (in the caller-supplied table or the runtime's
    /// own); the scan state lives in [`Runtime::mailboxes`](crate::runtime::Runtime).
    /// They are disjoint, so both are handed out mutably at once. A pid with no process
    /// is the typed [`crate::error::no_mailbox`] capability gap.
    /// Delivers `message` into `to`'s mailbox through the **heap-fragment** model — the
    /// sender half of the C `erts_try_alloc_message_on_heap`/`new_message_buffer` path
    /// (`erl_message.c:622-691`), reached when the receiver's own heap cannot hold the
    /// payload (`../../erts/emulator/beam/erl_message.c`).
    ///
    /// The payload is copied into a fresh [`HeapFragment`] sized to hold all of it, the
    /// fragment is pushed onto a [`Fragments`] chain, and the enqueued [`Message`] owns
    /// that chain, so its [`payload`](Message::payload) has a stable home the receiver's
    /// heap is not involved in. The off-heap objects the payload references are refcounted
    /// into the **fragment's** table (a fresh [`OffHeapTable`] as the copy's source — the
    /// same empty source [`beam_process`]'s own delivery uses), so the fragment owns them.
    ///
    /// The recipient is woken exactly as the ordinary path wakes it, except that the wake
    /// cannot go through the process table's wake list (that list is filled by
    /// `ProcessTable::deliver`, which this path deliberately bypasses): it is recorded on
    /// [`Runtime::note_fragment_wake`] instead, and
    /// [`Runtime::scheduler_pass`](crate::runtime::Runtime) absorbs it into the real run
    /// queue at the same points it absorbs the table's own wake list.
    ///
    /// # Why this is not `beam_message::send_fragmented`
    ///
    /// `send_fragmented` wants the receiver's **heap** *and* its **mailbox** at the same
    /// time, but [`beam_process::Process`] exposes them only through two `&mut self`
    /// accessors ([`heap_mut`](beam_process::Process::heap_mut) /
    /// [`mailbox_mut`](beam_process::Process::mailbox_mut)) — no split borrow — so the two
    /// cannot be handed out of `beam_rt` together. This method therefore composes the same
    /// public [`beam_message`] fragment primitives the function itself composes (the
    /// message is already known not to fit, so its preflight attempt is redundant anyway);
    /// see the crate `CONTEXT.md` handoff for the seam that would let it be deleted.
    ///
    /// # Errors
    ///
    /// The failures are **raw**: `HeapFragment::new`'s host-allocation failure and
    /// `copy_root`'s copy-engine exhaustion ([`AllocKind::Message`]). The single caller
    /// ([`Self::deliver_message`]) translates a genuine exhaustion to `error:system_limit`
    /// with [`error::enforce_term_heap_limit`]; that translation lives at the call site
    /// because the `Message` kind must stay recognizable as the retry trigger on the
    /// first `deliver_simple` attempt.
    fn send_fragmented_message(&mut self, to: ProcessId, from: Eterm, message: Eterm) -> Result<()> {
        // A fragment sized to hold the whole payload (the C sizes `new_message_buffer` the
        // same way). `size_object` is > 0 for any payload that failed the preflight.
        let mut fragment = HeapFragment::new(beam_term::size_object(message))?;
        // An empty source off-heap table: the sender's term carries no off-heap slot this
        // runtime can resolve (the engine seam hands over only the term), exactly as
        // `deliver_simple` assumes.
        let source = OffHeapTable::new();
        let root = fragment.copy_root(message, &source)?;
        let mut fragments = Fragments::new();
        fragments.push(fragment);
        let woken = {
            let process = self
                .process_table_mut()
                .get_mut(to)
                .ok_or(BeamError::TermNotFound)?;
            process
                .mailbox_mut()
                .enqueue(Message::new(root, from).with_fragments(fragments));
            process.wake()
        };
        if woken {
            self.runtime.note_fragment_wake(to);
        }
        Ok(())
    }

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
    /// Milestone-1's arena is unbounded (no GC yet), so this reports
    /// `usize::MAX` — see the module docs.
    pub fn heap_available(&self) -> usize {
        self.runtime.arena_available()
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
    /// # Errors
    ///
    /// [`crate::error::undef`] (class `error`, reason atom `undef`) when the function is
    /// not in the module's atom table or is not exported — exactly the raise the oracle
    /// produces for a call to a function the release does not have — plus any error the
    /// running code raises.
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
        let result = Machine::new()
            .run_export(module, function_atom, arity, args, self)
            .map_err(|err| error::name_atom_raise(err, self.runtime.atoms()));
        self.current_module = previous;
        result
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
        // The local-node identity: `nonode@nohost` with creation 0 (the M1 runtime has no
        // distribution), read from the shared table *before* it is borrowed by the context.
        // Both fields are `Copy`, so this hook needs no runtime borrow at all and every arm
        // below can install it (see [`NodeIdentity`]).
        let mut local_node = NodeIdentity {
            node: atoms.lookup("nonode@nohost"),
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

    /// Whether `(module, function, arity)` is one of the five process-dictionary BIFs that
    /// must run against the calling process's [`Dict`](beam_process::Dict).
    ///
    /// These BIFs take a `&mut ProcessTable` (the running process), which cannot co-exist
    /// with [`FunBridge`]'s `&mut Runtime` borrow — so [`Self::call_registered`] installs a
    /// [`DictBridge`] for them **instead of** the fun-caller bridge. No dict BIF applies a
    /// fun argument, so the fun-caller seam is never needed for one.
    fn uses_dict(module: &str, function: &str, arity: u32) -> bool {
        matches!(
            (module, function, arity),
            ("erlang", "put", 2)
                | ("erlang", "get", 1)
                | ("erlang", "erase", 1)
                | ("erlang", "get_keys", 0)
                | ("erlang", "get_keys", 1)
                | ("erlang", "get", 0)
                | ("erlang", "erase", 0)
        )
    }

    /// Whether `(module, function, arity)` is one of the eleven process/name/global-state
    /// BIFs that run against the [`ProcessBridge`] (the process-table and port-registry
    /// seam), so the runtime-native floor no longer serves them and no BIF is duplicated.
    fn uses_process_hook(module: &str, function: &str, arity: u32) -> bool {
        matches!(
            (module, function, arity),
            ("erlang", "whereis", 1)
                | ("erlang", "register", 2)
                | ("erlang", "unregister", 1)
                | ("erlang", "registered", 0)
                | ("erlang", "is_process_alive", 1)
                | ("erlang", "processes", 0)
                | ("erlang", "ports", 0)
                | ("erlang", "process_info", 1)
                | ("erlang", "process_info", 2)
                | ("erlang", "process_flag", 2)
                | ("erlang", "group_leader", 0)
        )
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
    /// [`Self::call_registered`] installs exactly one of the five. `load_nif/2` takes no fun
    /// argument and no dictionary, so nothing is lost.
    fn uses_nif_loader(module: &str, function: &str, arity: u32) -> bool {
        matches!((module, function, arity), ("erlang", "load_nif", 2))
    }

    /// Whether `(module, function, arity)` is `records:get_definition/2`, which must run
    /// against the [`RecordBridge`] (the record-source seam).
    ///
    /// The bridge borrows the code server's module map (`&Runtime`), so it cannot co-exist
    /// with [`FunBridge`]'s `&mut Runtime` or [`DictBridge`]'s `&mut ProcessTable` borrow —
    /// [`Self::call_registered`] installs exactly one of the five. `get_definition/2` takes no
    /// fun argument and no dictionary, so nothing is lost.
    fn uses_record_source(module: &str, function: &str, arity: u32) -> bool {
        matches!((module, function, arity), ("records", "get_definition", 2))
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
    fn uses_send_hook(module: &str, function: &str, arity: u32) -> bool {
        matches!((module, function, arity), ("erlang", "send", 2) | ("erlang", "!", 2))
    }

    /// Whether `(module, function, arity)` is `erlang:get_module_info/1,2`, which must run
    /// against the [`ModuleInfoBridge`] (the loaded-module-information seam).
    ///
    /// The bridge borrows the code server's module map (`&Runtime`), so it cannot co-exist
    /// with [`FunBridge`]'s `&mut Runtime` or [`SendBridge`]'s `&mut Runtime` borrow —
    /// [`Self::call_registered`] installs exactly one of them. `get_module_info/1,2` takes
    /// no fun argument and touches no dictionary, so nothing is lost.
    fn uses_module_info(module: &str, function: &str, arity: u32) -> bool {
        matches!((module, function, arity), ("erlang", "get_module_info", 1) | ("erlang", "get_module_info", 2))
    }

    /// Whether `(module, function, arity)` is one of the seven `persistent_term:*` BIFs,
    /// which must run against the [`PersistentTermBridge`] (the node-global store seam).
    ///
    /// The bridge holds a cheap clone of the runtime's shared store (`Rc<RefCell<…>>`), so
    /// it borrows **no** part of the runtime and can coexist with every other arm; it is an
    /// own `else if` arm only for symmetry and clarity. None of the seven takes a fun
    /// argument or a dictionary, so nothing is lost.
    fn uses_persistent_term(module: &str, function: &str, arity: u32) -> bool {
        matches!(
            (module, function, arity),
            ("persistent_term", "get", 0)
                | ("persistent_term", "get", 1)
                | ("persistent_term", "get", 2)
                | ("persistent_term", "put", 2)
                | ("persistent_term", "put_new", 2)
                | ("persistent_term", "erase", 1)
                | ("persistent_term", "info", 0)
        )
    }

    /// Whether `(module, function, arity)` is one of the runtime-coupled BIF families the
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
    /// [`RuntimeHooks`]: beam_bifs::RuntimeHooks
    fn uses_runtime_hooks(module: &str, function: &str, arity: u32) -> bool {
        matches!(
            (module, function, arity),
            ("erlang", "start_timer", 3)
                | ("erlang", "start_timer", 4)
                | ("erlang", "send_after", 4)
                | ("erlang", "cancel_timer", 2)
                | ("erlang", "function_exported", 3)
                | ("erlang", "link", 1)
                | ("erlang", "unlink", 1)
                | ("erlang", "dt_spread_tag", 1)
                | ("erlang", "dt_restore_tag", 1)
                | ("erlang", "statistics", 1)
        )
    }

    /// Whether `(module, function, arity)` is one of the two `erlang:memory/0,1` BIFs —
    /// the **only** consumers of the [`MemoryHook`](beam_bifs::MemoryHook) seam.
    ///
    /// These are not a bridge arm (the seam coexists with every arm) but a *build* gate:
    /// [`RuntimeMemory::new`] eagerly folds every loaded module's `.beam` image size, which
    /// is one `stat` syscall per loaded module, so building it on every registered call costs
    /// ~10 us per arithmetic BIF (see [`Self::call_registered`]). With no hook installed
    /// `beam_bifs`' `ctx.memory()` is the typed `unsupported_bif` capability gap, so the gate
    /// must cover **exactly** the keys that read the seam (workspace `CONTEXT.md` §3).
    fn uses_memory_hook(module: &str, function: &str, arity: u32) -> bool {
        matches!(
            (module, function, arity),
            ("erlang", "memory", 0) | ("erlang", "memory", 1)
        )
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
    fn uses_fun_identity(module: &str, function: &str, arity: u32) -> bool {
        matches!(
            (module, function, arity),
            ("erlang", "term_to_binary", 1)
                | ("erlang", "term_to_binary", 2)
                | ("erlang", "external_size", 1)
                | ("erlang", "external_size", 2)
        )
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
        let mut host =
            NifHostImpl::new(heap, Rc::clone(&atoms), interner.clone(), self_pid);
        // The registry is borrowed immutably only for the duration of the call, and released
        // before the interner is committed (which needs `&mut` on the code server).
        let outcome = {
            let nifs = self.runtime.nifs.borrow();
            let mut env = beam_abi::NifEnv::new(&mut host);
            nifs.call(module, function, arity, &mut env, args)
        };
        self.runtime.put_bif_heap(host.into_heap());
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

    /// Ensures `heap_need` words are available in the runtime's arena.
    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.runtime
            .ensure_heap(heap_need, live, x_regs, stack)
    }

    /// Heap words still available (see [`RuntimeContext::heap_available`]).
    fn heap_available(&self) -> usize {
        RuntimeContext::heap_available(self)
    }

    /// Allocates `n` raw heap words from the runtime's pointer-stable arena.
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.runtime.alloc_words(n)
    }

    /// Applies a fun/closure value.
    ///
    /// A fun built by the `-eval` evaluator (`uniq` == `eval::EVAL_FUN_UNIQ`) is applied
    /// through the evaluator's own store (`Runtime::eval_closures`): the fun may escape
    /// into a loaded module (`lists:map(fun(X) -> X*X end, …)`) which calls back into it,
    /// and the closure body can only be run by the evaluator. Every other fun is a
    /// `.beam` fun: it carries only its `uniq` (per-module) and `index` (the module-level
    /// lambda index); together they identify the [`FunDescriptor`] — hence the defining
    /// module and its entry label — unambiguously across every module the code server
    /// has loaded. The closure body is entered with the **free variables first**,
    /// followed by the call arguments (a lambda's captured variables are its leading
    /// parameters).
    ///
    /// [`FunDescriptor`]: beam_loader::FunDescriptor
    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        // An **external** fun (`fun m:f/A`, the `EXPORT_EXT` encoding) is not a closure at
        // all: it carries no `uniq`/`index` descriptor, so `terms::fun_uniq` rejects it with
        // `BadArg` and it must be recognised *before* that call. Its module/function/arity
        // are its whole identity, so applying it is an ordinary dynamic call through the
        // same dispatch every other call uses — the native floor, the registry, or a
        // load-on-demand module export. That is what makes `term_to_binary(fun
        // lists:reverse/1)` and `(fun lists:reverse/1)([1,2,3])` both work.
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
        let uniq = terms::fun_uniq(fun)?;
        if uniq == crate::eval::EVAL_FUN_UNIQ {
            let index = terms::fun_index(fun)? as usize;
            return crate::eval::apply_eval_closure(self, fun, index, args);
        }
        let index = terms::fun_index(fun)?;
        let mut combined = terms::fun_free(fun)?.to_vec();
        combined.extend_from_slice(args);

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
        Machine::new().run_label(&found, descriptor.label, descriptor.arity, &combined, self)
    }

    /// `send`: deliver `message` to the pid term `to`.
    ///
    /// `to` is resolved with [`ProcessId::from_eterm`] (a non-pid is [`BeamError::BadArg`])
    /// and the message is copied into the recipient's heap and enqueued by
    /// [`ProcessTable::deliver_simple`] (an unknown pid is the table's typed
    /// `TermNotFound`). The sender is this context's pid, so `self() ! M` reaches the very
    /// mailbox the following `receive` scans.
    ///
    /// A delivery that wakes a blocked recipient reports [`DeliverOutcome::Woke`]; the
    /// wake is **recorded on the process table's wake list** (not lost) and a later
    /// scheduler pass absorbs it by calling `Scheduler::resume_woken`. Milestone 1 has no
    /// scheduler loop, so nothing enqueues the woken process here — which is why a
    /// self-delivery is the `Enqueued` case this crate relies on.
    ///
    /// **A payload the recipient's heap cannot hold does not fail**: the copy engine
    /// preflights the receiver's young heap and reports
    /// [`Alloc(AllocKind::Message)`](beam_common::BeamError::Alloc) for a message larger
    /// than it, exactly as the C `erts_try_alloc_message_on_heap` does. That case is
    /// retried through the **heap-fragment** model ([`Self::send_fragmented_message`]),
    /// which is how the real system delivers a large message at all. A **genuine**
    /// exhaustion — the copy that still does not fit after the retry, or a non-message-heap
    /// allocation failure — becomes the emulator's documented `error:system_limit`
    /// ([`error::enforce_term_heap_limit`]), not the internal `allocation failure: …` text;
    /// the ordinary (small-payload) path is unchanged.
    ///
    /// A completed send also produces **events** (contract A.8, item 1): the ordinary
    /// `{trace, Sender, send, Msg, To}` — when the *sending* process is `[send]`-traced — and
    /// the sequential-trace `{seq_trace, Label, {send, …}}` when its token has the `send`
    /// flag. Both are delivered to the relevant tracer as messages of their own, after the
    /// payload ([`Self::trace_send_events`]). A third event follows when the **receiver** is
    /// `['receive']`-traced: `{trace, Receiver, 'receive', Msg}`, emitted here (never from the
    /// internal delivery bypass, which would recurse) by [`Self::trace_receive_events`].
    ///
    /// [`DeliverOutcome::Woke`]: beam_process::DeliverOutcome::Woke
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        let target = ProcessId::from_eterm(to)?;
        let from = self.mailbox_pid.to_eterm();
        self.deliver_message(target, from, message)?;
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