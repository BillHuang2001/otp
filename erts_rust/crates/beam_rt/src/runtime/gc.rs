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

//! The runtime's **process-heap collector adoption**: the safety state, the accounting and
//! the out-of-band term stabilization.
//!
//! A scheduled process allocates on its **own** [`beam_process::Process`] young heap and the
//! collector behind it (`beam_gc::AnyCollector`, reached through
//! [`Process::collect`](beam_process::Process::collect)) runs at the engine's
//! heap-admission point — see [`crate::context::gc`] for the GC point itself. This module
//! holds the state that decision needs and the two safety valves that keep the change
//! strictly behaviour-preserving:
//!
//! * the **pinning** set ([`GcState::pinned`]) — a pid whose engine float-register file was
//!   observed non-`NIL` is never collected again, because `Machine::fr` holds *terms* at
//!   Milestone 1 and is not part of the root set `ensure_heap` hands out;
//! * the **halt** set ([`GcState::halted`]) — a pid whose collection *failed* (or whose
//!   root-set sizes did not survive a collection) stops collecting, because a failing
//!   collection must never fail the program where the unbounded arena could not;
//! * the global **valve** ([`GcState::disabled`]) — tripped by
//!   [`Runtime::try_stabilize_escaped`] when a runtime-level store holds a term with an
//!   **off-heap payload** this crate's copy engine cannot duplicate. From then on **no**
//!   process heap is moved again, so the store's term can never dangle. A term that
//!   *genuinely* cannot be copied is **not** a valve case: it is reported honestly (the
//!   emulator's `error:system_limit`).
//!
//! Everything here is counters plus a few small hash sets and one shared per-pid descent latch
//! cell, deliberately: the runtime is single-threaded and these are read on the dispatch path,
//! not the hot path (the GC point is only armed inside
//! `runner::budget::BudgetContext::ensure_heap`).
//!
//! # Wiring
//!
//! The state is owned by [`Runtime`] (a `gc: GcState` field), the collection point is
//! [`crate::context::gc::ensure_heap`] (armed by the slice's own
//! [`BudgetContext::ensure_heap`](crate::runner)), and every escape store — the runner's
//! exit-reason stabilization included — goes through [`Runtime::try_stabilize_escaped`]. See
//! those call sites for the details.
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_gc::CollectionKind;
use beam_loader::Module;
use beam_process::{ProcessId, ProcessTable};
use beam_term::{make_atom, Eterm};

use super::{Runtime, SourcePayloads};

use crate::context::gc_walk::{ForeignLatch, WalkScratch};
use crate::context::RuntimeContext;

/// A snapshot of this runtime's process-heap collection activity.
///
/// The counters are **measured own-state** figures (they describe this runtime's own
/// collector, not the oracle's) and exist so a test and `erlang:statistics(garbage_collection)`
/// can see that the collector really ran, and *why it did not* when it did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcStatsSnapshot {
    /// How many collections ran (minor **and** major).
    pub collections: u64,
    /// How many of [`collections`](GcStatsSnapshot::collections) were fullsweeps.
    pub major: u64,
    /// How many words those collections reclaimed — the from-space words a collector **freed**
    /// rather than copied, accumulated from each collection's own outcome (never an estimate).
    pub reclaimed: u64,
    /// How many pids are permanently pinned (a live float register was observed).
    pub pinned: usize,
    /// How many pids have their collection halted (a collection failed or mis-sized).
    pub halted: usize,
    /// Whether process-heap collection is globally disabled (the escape-store valve).
    pub disabled: bool,
    /// How many engine allocations fell back to the pointer-stable arena.
    pub fallbacks: u64,
    /// How many collections failed (each one halting its own process).
    pub errors: u64,
    /// How many escape-store stabilizations tripped the global valve.
    pub valve_trips: u64,
}

/// One **un-discharged** asynchronous `erlang:garbage_collect(Pid, [{async, ReqId}])` request.
///
/// The wrapper returns the atom `async` immediately and the target's collection owes the
/// **requester** the message `{garbage_collect, ReqId, GCResult}` — the C's
/// `notify_sys_task_executed` (`erts/emulator/beam/erl_process.c:10574-10623`). This record is
/// what the runtime needs to build that message when the request is discharged
/// ([`Runtime::deliver_gc_completions`]).
pub(crate) struct AsyncGcCompletion {
    /// The **target** of the request — the process whose liveness the completion reports, and
    /// the identity a later request for the same target replaces.
    pub(crate) target: ProcessId,
    /// The process that made the call — `{garbage_collect, ReqId, Result}` is sent to it.
    pub(crate) requester: ProcessId,
    /// The caller's `ReqId` (any term), copied onto the runtime's pointer-stable arena by
    /// [`Runtime::stabilize_term`] before it is recorded, so it outlives both the caller's own
    /// (movable) young heap and the BIF heap the option list was parsed from. It is read back
    /// only when the completion message is built.
    pub(crate) req_id: Eterm,
}

/// The runtime's process-heap collection state (see the module docs).
pub(crate) struct GcState {
    /// Per-module answer to "may this module's code be collected over?" — `true` when the
    /// module contains **no** float-family instruction (the float guard).
    ///
    /// Keyed by module name; a module's code never changes once loaded, so the answer is
    /// cached for the runtime's lifetime.
    float_free: HashMap<String, bool>,
    /// Pids that must never be collected again (a live float register was observed).
    pinned: HashSet<ProcessId>,
    /// Pids whose collection is halted (a collection failed, or the root-set sizes changed).
    halted: HashSet<ProcessId>,
    /// Pids with an **unserved** forced-GC request — the on-demand collection behind
    /// `erlang:garbage_collect/0,1` and `erts_internal:garbage_collect/1`, recorded by the
    /// BIF and served at a point that owns the executing machine's root set (see
    /// [`Runtime::request_gc`]).
    requested: HashSet<ProcessId>,
    /// The reductions each pid has accumulated since its last **reduction/yield-boundary**
    /// collection ([`crate::context::gc::collect_at_reds_boundary`]).
    ///
    /// The interval gate of the slice-end door: a pre-emption may collect a process's heap at
    /// most once per [`crate::context::gc::REDS_BOUNDARY_MIN_REDS`] of accumulated work, so a
    /// pid is absent here until its first boundary and an entry is removed the moment the door
    /// resets it. A `HashMap` rather than a set: the entry carries the *count*, so several
    /// short slices add up to one boundary. A terminated pid's entry is pruned with the forced
    /// requests ([`Runtime::prune_gc_requests`]) — its heap is gone, so the count is noise.
    reds_since_boundary: HashMap<ProcessId, u32>,    /// The **asynchronous** `erlang:garbage_collect/2` requests still owed a completion
    /// message, in **request order**, at most one entry per **target** pid (see
    /// [`AsyncGcCompletion`]).
    ///
    /// A record is dropped at the first pass boundary after the request
    /// ([`Runtime::deliver_gc_completions`]), which also emits the message: the target is then
    /// either still alive (the oracle's `true`) or gone (the oracle's `false`).
    ///
    /// A **`Vec`**, not a map: the drain must emit the completions in **request order**, as the
    /// oracle does, where a `HashMap`'s randomised per-process iteration order would make the
    /// order run-to-run varying. A new request for a target that already has one **replaces
    /// that entry in place** (keeping its position), mirroring the one-bit-per-process latch of
    /// [`Runtime::request_gc`].
    async_completions: Vec<AsyncGcCompletion>,
    /// The **descent gate**: the pids that have ever had a container from a region their
    /// collector does not scan enter their reachable term graph.
    ///
    /// This is a **shared** per-pid latch cell (`Rc<RefCell<…>>`): the installed
    /// [`RuntimeCollectionGate`](crate::context::gc_walk::RuntimeCollectionGate) holds a clone
    /// of it, so an admission point here and a `beam_process` signal site read and write the
    /// **same** bits. [`Runtime::foreign_latch`] hands out one more clone of it.
    ///
    /// [`any_root_is_unscanned`](crate::context::gc_walk) is `O(live objects)` when it
    /// descends, so the descent is skipped unless this per-process bit is set — see that
    /// module's "descent gate" section for the soundness argument. In one line: the walk's
    /// always-run per-word phase already classifies every **root**, so only the *nested* shape
    /// needs the descent, and a foreign container can only become a scanned container's child
    /// by being *held* first — which is exactly what the events that call
    /// [`Runtime::note_foreign_reachable`] record (an engine arena fallback, a BIF/floor/NIF
    /// result outside the process heap, a heap-fragment delivery, a spawn argument that is not
    /// the new process's own).
    ///
    /// The bit is **scoped to the foreign containers actually in the graph, not to the process's
    /// life**: it is set by the events above and — the other half — **closed** again by a
    /// full-root descent that reached no container from an unscanned region at all
    /// ([`Runtime::close_foreign_reachable`]), which is what a loop that drops its foreign term
    /// (or never nested one) relies on to stop paying the `O(live)` descent. Only the two
    /// *full* admission doors may close it: they hand the valve the whole `RootSet`, so "no
    /// foreign container reachable" is a statement about the process's entire term graph, while
    /// the narrow door, the park arm and the signal gate see less. A clear is conservative in
    /// exactly one direction: an under-approximated root set would skip a descent, so the two
    /// doors that cannot see everything never clear.
    ///
    /// The cell is a `HashSet<ProcessId>` over the crate's own [`crate::hasher`] hasher rather
    /// than SipHash: it is a **verdict-only** set of internal pids (probed once per admission
    /// point, inserted into on a decline, removed by a clean full-root descent — see
    /// [`Runtime::note_foreign_reachable`]), never iterated for an observable ordered result, so
    /// the seeded hash's cost buys nothing here. See the `hasher` module docs for the soundness
    /// argument.
    foreign: ForeignLatch,
    /// The reachability walk's **reused** scratch (its worklist and its visited set), so a
    /// collection never allocates the descent's structures (see
    /// [`WalkScratch`](crate::context::gc_walk::WalkScratch)).
    walk: WalkScratch,
    /// Whether **every** process heap must stay put (the global escape-store valve).
    disabled: bool,
    /// Why the global valve tripped (the first reason wins), for diagnostics.
    disabled_reason: Option<&'static str>,
    /// How many escape stores tripped the valve.
    valve_trips: u64,
    /// Collections run (minor and major).
    collections: u64,
    /// Fullsweeps among them.
    major: u64,
    /// The words those collections reclaimed, summed from each collection's own
    /// [`Collection`](beam_gc::Collection) outcome (see [`GcStatsSnapshot::reclaimed`]).
    reclaimed: u64,
    /// Engine allocations that fell back to the arena.
    fallbacks: u64,
    /// Collections that failed.
    errors: u64,
    /// The reusable **placeholder fill** the safe heap bump needs (see
    /// [`crate::context::gc::bump`]): a slice of [`NIL`](beam_term::NIL) of the largest size
    /// asked for so far, so a bump does not allocate.
    ///
    /// The words it holds are never a term graph — they are overwritten by the engine before
    /// the block they pad is ever reachable — and it is deliberately *not* a GC root.
    scratch: Vec<Eterm>,
}

impl GcState {
    /// A fresh state: nothing collected, nothing pinned, the valve closed.
    pub(crate) fn new() -> GcState {
        GcState {
            float_free: HashMap::new(),
            pinned: HashSet::new(),
            halted: HashSet::new(),
            requested: HashSet::new(),
            reds_since_boundary: HashMap::new(),            async_completions: Vec::new(),
            foreign: ForeignLatch::default(),
            walk: WalkScratch::default(),
            disabled: false,
            disabled_reason: None,
            valve_trips: 0,
            collections: 0,
            major: 0,
            reclaimed: 0,
            fallbacks: 0,
            errors: 0,
            scratch: Vec::new(),
        }
    }

    /// A fresh state that observes `latch` as its descent gate — the form the runtime's install
    /// site builds, so this state and the installed
    /// [`RuntimeCollectionGate`](crate::context::gc_walk::RuntimeCollectionGate) read and write
    /// the **one** shared per-pid cell ([`Runtime::foreign_latch`] hands out one more clone).
    pub(crate) fn with_latch(latch: ForeignLatch) -> GcState {
        let mut state = GcState::new();
        state.foreign = latch;
        state
    }
}

// The wiring lands the call sites incrementally (the collection point first, then the
// escape-store stabilization), so a helper here whose caller has not arrived yet is
// legitimately unused for now — `gc_disabled_reason` is the one such diagnostic.
impl Runtime {
    /// The runtime's collection counters and safety state.
    ///
    /// These are **node-global** figures: they count the work this runtime really did since it
    /// started, independently of which processes are live at the read — the shape
    /// `erlang:statistics(garbage_collection)` needs (the C's per-scheduler accumulators outlive
    /// every process; summing live processes' collectors cannot, and reads *nothing* at all while
    /// the scheduler has the process table moved out for a slice).
    pub fn gc_stats(&self) -> GcStatsSnapshot {
        GcStatsSnapshot {
            collections: self.gc.collections,
            major: self.gc.major,
            reclaimed: self.gc.reclaimed,
            pinned: self.gc.pinned.len(),
            halted: self.gc.halted.len(),
            disabled: self.gc.disabled,
            fallbacks: self.gc.fallbacks,
            errors: self.gc.errors,
            valve_trips: self.gc.valve_trips,
        }
    }

    /// Whether **any** process heap may be moved — the global valve is closed and the
    /// runtime has not tripped it.
    pub(crate) fn gc_enabled(&self) -> bool {
        !self.gc.disabled
    }

    /// Whether `pid`'s own process heap may be moved (it is not pinned, not halted, and the
    /// global valve is closed).
    pub(crate) fn gc_process_enabled(&self, pid: ProcessId) -> bool {
        // The per-pid valves are read on **every** heap-admission point, before the occupancy
        // short-circuit, and in a normal workload both sets are empty — so answer the empty
        // case with a length compare instead of a (SipHash) probe that can only miss. The
        // verdicts are byte-for-byte the ones `!set.contains(&pid)` gives for every input.
        self.gc_enabled()
            && (self.gc.pinned.is_empty() || !self.gc.pinned.contains(&pid))
            && (self.gc.halted.is_empty() || !self.gc.halted.contains(&pid))
    }

    /// Records a **forced-GC request** for `pid` — the on-demand collection behind
    /// `erlang:garbage_collect/0,1` and `erts_internal:garbage_collect/1`.
    ///
    /// A BIF may not collect: the root set a collection relocates is the **executing
    /// machine's** `x[0..live)` plus its Erlang stack, and a BIF runs *inside*
    /// [`ExecutionContext`](beam_engine::ExecutionContext), which owns neither. So the BIF
    /// records the request here, answers the oracle's `true`, and the runtime serves the
    /// request at the two points that *do* own that root set:
    ///
    /// * the engine's heap-admission point ([`crate::context::gc::ensure_heap`]) — the
    ///   slice's **own** machine, whose registers it is handed — so a loop that keeps
    ///   allocating collects on every request; and
    /// * the runner's park arms ([`Runtime::serve_gc_request`]), where the machine's whole
    ///   live root set has just been published into the process — so a process that asks for
    ///   a collection and then blocks (or is pre-empted) is still served.
    ///
    /// A process that reaches **neither** after asking (it returns without allocating and
    /// without parking) leaves the request unserved; its heap dies with it, so nothing is
    /// collected and nothing is pretended (the requests of terminated processes are pruned
    /// at the pass boundary, [`Runtime::prune_gc_requests`]).
    ///
    /// The request is one bit per process: asking twice before it is served collects once,
    /// the honest reading of "that process was already due a collection".
    pub(crate) fn request_gc(&mut self, pid: ProcessId) {
        self.gc.requested.insert(pid);
    }

    /// Records an **asynchronous** `erlang:garbage_collect(Pid, [{async, ReqId}])` request: the
    /// forced-GC request itself (when `live`, exactly as [`Runtime::request_gc`]) **plus** the
    /// completion the target's collection owes `requester`.
    ///
    /// The wrapper answers the atom `async` and returns; the requester is later sent
    /// `{garbage_collect, ReqId, GCResult}` — the C's `notify_sys_task_executed`
    /// (`erts/emulator/beam/erl_process.c:10574-10623`), whose `GCResult` is `am_true` for a live
    /// target and `am_false` when the target is already exiting/gone
    /// (`liveness_notify_sys_task_executed`, `erl_process.c:11488-11490`).
    ///
    /// `live` is whether the caller's process table holds `pid`: a **dead** target has no heap to
    /// collect, so no request is armed (nothing is ever collected for a pid that cannot be), but
    /// the completion is still owed — the oracle's `async` reply is followed by the `false`
    /// message.
    ///
    /// `req_id` must already be a **pointer-stable** term
    /// ([`Runtime::stabilize_term`]): the caller's young heap may move (or die) long before the
    /// message is built. A **later** async request for the same target replaces an earlier one,
    /// mirroring the one-bit-per-process latch of [`Runtime::request_gc`].
    pub(crate) fn request_gc_async(
        &mut self,
        pid: ProcessId,
        requester: ProcessId,
        req_id: Eterm,
        live: bool,
    ) {
        if live {
            self.gc.requested.insert(pid);
        }
        // One outstanding completion per target: a later request for the same target replaces
        // the earlier entry **in place**, so the replace never reorders the outstanding requests.
        match self
            .gc
            .async_completions
            .iter_mut()
            .find(|completion| completion.target == pid)
        {
            Some(existing) => {
                existing.requester = requester;
                existing.req_id = req_id;
            }
            None => self.gc.async_completions.push(AsyncGcCompletion {
                target: pid,
                requester,
                req_id,
            }),
        }
    }

    /// Emits the `{garbage_collect, ReqId, Result}` completion of every **asynchronous**
    /// `erlang:garbage_collect/2` request ([`Runtime::request_gc_async`]) into its requester's
    /// mailbox, returning how many were delivered.
    ///
    /// This is the emission half of the async request, and it runs at a **scheduler-pass
    /// boundary** (`crate::runtime::scheduler`). The BIF itself cannot emit: a receiver-side
    /// delivery may collect the *receiver's* heap (`Process::deliver`'s rooted-collection path),
    /// and a BIF runs inside the executing machine, whose root set is in the engine's own Rust
    /// locals — the same reason a BIF may not collect (see [`Runtime::request_gc`]). The runner's
    /// park arms are the other point that owns a root set, but they can only serve *their own*
    /// pid, and the request the runner serves belongs to the very process being parked; emitting
    /// there for those cases and here for everything else would split the one rule in two.
    ///
    /// # The result is the oracle's liveness answer, not a collection report
    ///
    /// ERTS replies `am_true` whenever the GC system task ran for a **live** target and
    /// `am_false` only when the target was exiting/gone
    /// (`liveness_notify_sys_task_executed`, `erts/emulator/beam/erl_process.c:11488-11490`) —
    /// the reply is a *liveness* fact, not a report about the collector's own work. So the result
    /// here is `table.contains(target)`, the same fact the oracle reports. The collection the
    /// request armed is **not** lost by emitting first: the latch
    /// ([`GcState::requested`]) stays armed and is served at the target's own next collection
    /// site, exactly as a synchronous `/1` request is (and, like it, is dropped if a valve
    /// refuses the move).
    ///
    /// A requester that has itself terminated has nobody to notify: the completion is dropped,
    /// exactly as the C's `notify_sys_task_executed` drops it when `erts_pid2proc` finds no
    /// process. A message that cannot be delivered (the requester's heap genuinely exhausted) is
    /// the honest [`BeamError::SystemLimit`], propagated to the pass's caller — never silently
    /// discarded and never a fabricated `true`.
    ///
    /// # Errors
    ///
    /// The term builder's allocation failure and the delivery path's own errors (translated to
    /// the emulator's documented `error:system_limit`).
    pub(crate) fn deliver_gc_completions(&mut self, table: &mut ProcessTable) -> Result<usize> {
        if self.gc.async_completions.is_empty() {
            return Ok(0);
        }
        // Drained outright: every record is discharged here (the request was latched, the result
        // is the target's liveness at this boundary), so nothing may be left behind for a later
        // boundary to re-report. The drain keeps the insertion (**request**) order.
        let pending: Vec<AsyncGcCompletion> = self.gc.async_completions.drain(..).collect();
        let mut delivered = 0;
        for completion in pending {
            if !table.contains(completion.requester) {
                continue;
            }
            let message =
                self.gc_completion_message(completion.req_id, table.contains(completion.target))?;
            // The sender `from` is the emulator itself — the C queues the task reply as a
            // message of the emulator (`ERL_MESSAGE_TOKEN(mp) = am_undefined`, with the sending
            // process never consulted for the envelope), and an *immediate* is required because
            // the envelope is not copied nor gathered as a GC root (`beam_message::Message::from`).
            let from = make_atom(self.code_server.atoms_mut().intern("system"));
            RuntimeContext::deliver_in_table(self, table, completion.requester, from, message)?;
            delivered += 1;
        }
        Ok(delivered)
    }

    /// Builds the `{garbage_collect, ReqId, Result}` message the async completion is delivered
    /// as — `TUPLE3(hp, st->reply_tag, req_id, st_result)` of the C's `notify_sys_task_executed`
    /// (`erts/emulator/beam/erl_process.c:10618`), with the reply tag `am_garbage_collect` and
    /// the `true`/`false` result atoms.
    ///
    /// The tuple is built on the runtime's **pointer-stable** arena
    /// ([`RuntimeContext`]'s heap, which is never moved and never reset), so the delivery's own
    /// receiver-heap copy is the only copy the message ever takes.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the tuple does not fit.
    fn gc_completion_message(&mut self, req_id: Eterm, result: bool) -> Result<Eterm> {
        let mut ctx = RuntimeContext::new(self);
        let (tag, result) = {
            let atoms = ctx.runtime.code_server.atoms_mut();
            (
                make_atom(atoms.intern("garbage_collect")),
                make_atom(atoms.intern(if result { "true" } else { "false" })),
            )
        };
        terms::make_tuple(&mut ctx, &[tag, req_id, result])
    }

    /// Whether `pid` has an **unserved** forced-GC request (see [`Runtime::request_gc`]).
    pub(crate) fn gc_request_pending(&self, pid: ProcessId) -> bool {
        // Empty case (the common one) == a length compare; see `gc_process_enabled`.
        !self.gc.requested.is_empty() && self.gc.requested.contains(&pid)
    }

    /// Consumes `pid`'s forced-GC request, answering whether there was one.
    ///
    /// A **served** request is consumed when the collection is committed to (the admission
    /// point) or attempted (the park arm), and a **refused** one is dropped by
    /// [`Runtime::gc_process_enabled`]'s valves rather than left armed: the BIF already
    /// answered the oracle's `true` (the request was accepted), and re-arming would
    /// silently collect the process the moment a valve happened to open — the opposite of
    /// what an intentionally-collecting runtime should do.
    pub(crate) fn take_gc_request(&mut self, pid: ProcessId) -> bool {
        self.gc.requested.remove(&pid)
    }

    /// Serves `pid`'s pending forced-GC request **now** — the runner's park arm (see
    /// [`Runtime::request_gc`] for why the BIF itself cannot collect).
    ///
    /// This is a legitimate collection site: by the time the walk below runs, `pid` is **not**
    /// relocation-exempt (the check above returns `false` first), and a park that is not exempt
    /// is exactly one that published the parked machine's whole live root set into `pid`
    /// ([`publish_parked_roots`](crate::runner) — the `Yield` arm's park with a dead
    /// float-register prefix). The relocation-exempt windows are the parks that **skip** that
    /// publication — the `Wait` arm's parked window, and a `Yield` arm whose slice ended with a
    /// live FR prefix — and each is refused right above, so no path reaches the walk with an
    /// unpublished root set. The machine's float-register file is *provably* dead here — the
    /// post-slice [`Machine::fr_mark`](beam_engine::Machine::fr_mark)
    /// check opens the window exemption for any pid whose `Machine::fr` holds a term,
    /// so a pid that is still [`gc_process_enabled`](Runtime::gc_process_enabled) cannot be
    /// carrying one. The collector therefore relocates exactly what is live, against the
    /// process's own root set.
    ///
    /// Returns whether a collection ran. The answer is `false` — with the counters
    /// untouched, never a fabricated collection — for a request the valves refuse (a pinned
    /// or halted pid, the global escape-store valve, or a live root that reaches a container in
    /// a region the collector does not scan), for a pid the table no longer holds,
    /// and for a **relocation-exempt** process (the host holds live words outside this
    /// crate's root set: nothing is moved and, because nothing failed, the process's
    /// collection is *not* halted). A genuine collection failure halts that process's
    /// collection ([`Runtime::halt_process_gc`]), exactly as the admission point does.
    pub(crate) fn serve_gc_request(&mut self, table: &mut ProcessTable, pid: ProcessId) -> bool {
        if !self.take_gc_request(pid) {
            return false;
        }
        if !self.gc_process_enabled(pid) {
            return false;
        }
        let Some(process) = table.get_mut(pid) else {
            return false;
        };
        if process.is_relocation_exempt() {
            return false;
        }
        // The **unscanned-root valve** ([`crate::context::gc_walk`]) — the same valve the
        // admission point and the collecting allocation seam consult, here over the process's
        // own root set (the pid is not relocation-exempt here, so this is a park that did publish
        // the parked machine's roots into it, and a delivery
        // may have filled its mailbox since). A container in a region the collector does not
        // scan, reachable from a live root, makes the move unsound, so it is declined: the
        // request is dropped exactly as the other valves drop it, and nothing is moved.
        //
        // The descent is gated on the pid's **foreign-reachability** bit (see
        // [`GcState::foreign`]): a root that is itself foreign is still caught by the walk's
        // always-run per-word phase, so the gate only ever removes the `O(live)` descent.
        //
        // The `hit` arm opens that bit permanently: a foreign container *did* reach a live root
        // of this process, which is exactly the event the gate records (it can only ever make a
        // later valve more conservative). See [`Runtime::walk_scratch`].
        let pid_foreign = self.foreign_reachable(pid);
        #[cfg(test)]
        crate::context::gc::valve_counters::note_reached(
            crate::context::gc::valve_counters::Door::ServeGcRequest,
            false,
            false,
            false,
            0,
        );
        let declined = crate::context::gc_walk::process_roots_are_unscanned(
            self.walk_scratch(),
            process,
            pid_foreign,
        );
        #[cfg(test)]
        crate::context::gc::valve_counters::note_valve(
            crate::context::gc::valve_counters::Door::ServeGcRequest,
            pid_foreign,
            declined,
        );
        if declined {
            self.note_foreign_reachable(pid);
            return false;
        }
        match process.collect() {
            Ok(collection) => {
                self.note_gc_collection(matches!(collection.kind, CollectionKind::Major));
                self.note_gc_reclaimed(collection.stats.words_reclaimed);
                #[cfg(test)]
                crate::context::gc::valve_counters::note_collected(
                    crate::context::gc::valve_counters::Door::ServeGcRequest,
                );
                true
            }
            Err(_) => {
                self.halt_process_gc(pid);
                false
            }
        }
    }

    /// Drops the per-process GC state of processes `table` no longer holds: the forced-GC
    /// requests a terminated process never reached a collection site for — its heap is gone —
    /// and the **foreign-reachability gate** of a pid that no longer exists.
    ///
    /// A process that terminated with a request still pending (it reached no heap-admission
    /// point and no park after asking) can never be served — its heap is gone — so the entry
    /// is noise that would otherwise accumulate for the runtime's whole lifetime, one per
    /// such process. The scheduler pass prunes at its boundary, where the pass's own table
    /// holds exactly the live processes.
    ///
    /// An **asynchronous** request ([`Runtime::request_gc_async`]) that is pruned here is not
    /// silently forgotten: the completion it owes its requester reports the target as gone
    /// (`false`), which is exactly what the C does, and is emitted by
    /// [`Runtime::deliver_gc_completions`] — the pass runs it at this same boundary.
    pub(crate) fn prune_gc_requests(&mut self, table: &ProcessTable) {
        if !self.gc.requested.is_empty() {
            self.gc.requested.retain(|pid| table.contains(*pid));
        }
        if !self.gc.reds_since_boundary.is_empty() {
            self.gc.reds_since_boundary.retain(|pid, _| table.contains(*pid));
        }
        if !self.gc.foreign.borrow().is_empty() {
            self.gc.foreign.borrow_mut().retain(|pid| table.contains(*pid));
        }
    }
    /// Adds `reds` to the reductions `pid` has accumulated since its last
    /// **reduction/yield-boundary** collection, returning the new total.
    ///
    /// The interval half of the slice-end door
    /// ([`crate::context::gc::collect_at_reds_boundary`]): the caller passes the reduction count
    /// its just-ended slice spent and gets back the pid's running total, which it compares
    /// against [`crate::context::gc::REDS_BOUNDARY_MIN_REDS`]. Adding up (rather than replacing)
    /// is what makes several short pre-emptions equal one boundary, so a process cannot avoid
    /// the door by yielding early every time.
    ///
    /// Saturating: a process that somehow outlived `u32::MAX` accumulated reductions is over any
    /// boundary — never a panic, never a wrap back below the threshold.
    pub(crate) fn accumulate_reds_since_gc_boundary(&mut self, pid: ProcessId, reds: u32) -> u32 {
        let total = self
            .gc
            .reds_since_boundary
            .entry(pid)
            .and_modify(|total| *total = total.saturating_add(reds))
            .or_insert(reds);
        *total
    }
    /// Clears `pid`'s accumulated reduction interval — the boundary was just reached and the
    /// door ran (or declined past the interval gate, which is the same thing from the interval's
    /// point of view: the next window starts here).
    pub(crate) fn reset_reds_since_gc_boundary(&mut self, pid: ProcessId) {
        if self.gc.reds_since_boundary.is_empty() {
            return;
        }
        self.gc.reds_since_boundary.remove(&pid);
    }
    /// Records that a collection ran.
    ///
    /// This is the **kind-only** half: the engine's heap-admission doors
    /// ([`crate::context::gc`]) hold the machine's root set but report only whether the
    /// collection was a fullsweep, so they call this one. A site that also holds the
    /// collection's own outcome records its reclaimed words through
    /// [`Runtime::note_gc_reclaimed`].
    pub(crate) fn note_gc_collection(&mut self, major: bool) {
        self.gc.collections += 1;
        if major {
            self.gc.major += 1;
        }
    }

    /// Records the words one collection reclaimed: the from-space words it **freed** instead of
    /// copying, read from the collection's own outcome
    /// ([`Collection::stats`](beam_gc::Collection::stats) `words_reclaimed`) — never an estimate.
    ///
    /// Summed into [`GcStatsSnapshot::reclaimed`], which `erlang:statistics(garbage_collection)`
    /// reports as its `WordsReclaimed` element.
    pub(crate) fn note_gc_reclaimed(&mut self, words: usize) {
        // A `usize` word count is a small integer on any platform this runtime builds on; the
        // saturation keeps the conversion total without trapping.
        self.gc.reclaimed += u64::try_from(words).unwrap_or(u64::MAX);
    }

    /// Records that an engine allocation for `pid` could not be served from a process young
    /// heap and fell back to the pointer-stable arena.
    ///
    /// It also **opens `pid`'s descent gate** ([`GcState::foreign`]): the block the caller is
    /// handed comes from a region the collector does not scan, and the engine writes that
    /// process's own terms into it, so from this moment the process's graph can hold a foreign
    /// container whose children the copier would leave behind. Every fallback to the arena is
    /// recorded the same way, so the gate can never be closed while such a block is reachable.
    pub(crate) fn note_gc_fallback(&mut self, pid: ProcessId) {
        self.gc.fallbacks += 1;
        // The set's own verdict (already computed by `insert`) is the latch counters' witness, so
        // the hot path pays nothing for it: a release build just binds it away.
        #[cfg_attr(not(test), allow(unused_variables))]
        let fresh = self.gc.foreign.borrow_mut().insert(pid);
        #[cfg(test)]
        latch_counters::note_insert(fresh);
        #[cfg(test)]
        crate::context::gc::valve_counters::note_fallback();
    }

    /// Opens `pid`'s **descent gate** ([`GcState::foreign`]): from now on the unscanned-root
    /// valve descends through the process's scanned graph.
    ///
    /// Called by the events that can put a container from an unscanned region into `pid`'s
    /// reachable graph — the engine's arena fallback ([`Runtime::note_gc_fallback`]), a BIF /
    /// runtime-native / NIF result built outside the process heap that **forwards a scanned
    /// word** ([`open_gate_for_foreign_result`](crate::RuntimeContext)), a heap-fragment message
    /// delivery, a spawn argument that is not the new process's own — and by the valve itself
    /// when it observes a **hazard** at a live root. The bit is a hint, never a decision: while
    /// it is open every candidate collection also runs the `O(live)` descent, and while it is
    /// closed the always-run per-word phase still classifies every root. Its only cost direction
    /// is to *add* conservatism, and it is closed again by a clean full-root descent
    /// ([`Runtime::close_foreign_reachable`]).
    ///
    /// This is one of the two hot callers of the latch (the other is
    /// [`Runtime::foreign_reachable`]): the valve **declines** on essentially every allocating
    /// instruction of a process that holds an arena/BIF-heap block, so the insert's own verdict is
    /// taken rather than thrown away (test builds count it — see [`latch_counters`] — and the
    /// probe itself is the crate's own [`crate::hasher`] word mix, not SipHash).
    pub(crate) fn note_foreign_reachable(&mut self, pid: ProcessId) {
        #[cfg_attr(not(test), allow(unused_variables))]
        let fresh = self.gc.foreign.borrow_mut().insert(pid);
        #[cfg(test)]
        latch_counters::note_insert(fresh);
    }

    /// Closes `pid`'s **descent gate** ([`GcState::foreign`]) after a descent that ran over the
    /// process's **whole** root set and reached no container from a region the collector does not
    /// scan.
    ///
    /// This is the *only* place the bit is cleared, and it is deliberately narrow: the caller must
    /// have handed the valve the executing machine's complete `RootSet` (the two full admission
    /// doors, [`crate::context::gc`]) and received a
    /// [`ValveVerdict::foreign_free`](crate::context::gc_walk::ValveVerdict) — the walk itself
    /// only reports that property for a walk that descended over the roots it was handed. "No
    /// foreign container is reachable" is what makes the clear sound: a hazard needs a foreign
    /// container to *hold* a scanned word, so with none reachable there is none to leave dangling,
    /// and the graph can only acquire one through an event that re-opens the bit.
    ///
    /// Answering whether the bit was open, for the test-only witnesses.
    pub(crate) fn close_foreign_reachable(&mut self, pid: ProcessId) -> bool {
        if self.gc.foreign.borrow().is_empty() {
            return false;
        }
        #[cfg_attr(not(test), allow(unused_variables))]
        let closed = self.gc.foreign.borrow_mut().remove(&pid);
        #[cfg(test)]
        latch_counters::note_remove(closed);
        closed
    }

    /// Whether `pid`'s descent gate is open (see [`GcState::foreign`]) — i.e. whether the
    /// unscanned-root valve must descend through `pid`'s scanned graph rather than run only its
    /// per-word root phase.
    pub(crate) fn foreign_reachable(&self, pid: ProcessId) -> bool {
        // The `RefCell` borrow guard is itself per-call cost, and the cell is empty in a
        // normal workload — so borrow **once**, answer the empty case with a length compare,
        // and probe only when the bit could really be there. `contains`'s verdict, unchanged.
        let foreign = self.gc.foreign.borrow();
        if foreign.is_empty() {
            return false;
        }
        #[cfg(test)]
        latch_counters::note_probe();
        foreign.contains(&pid)
    }

    /// The runtime's **one** shared per-pid descent latch cell (see [`GcState::foreign`]).
    ///
    /// It is the same cell the installed
    /// [`RuntimeCollectionGate`](crate::context::gc_walk::RuntimeCollectionGate) reads, so
    /// [`Runtime::note_foreign_reachable`] opening a pid's bit is exactly what makes a
    /// `beam_process` signal site decide in `O(1)` for that pid.
    ///
    /// Test-only: production code always reaches the cell as `GcState::foreign`'s own field
    /// (the mutators) or through the gate that holds a clone of it, so this is a witness for
    /// the unit suite rather than a production entry point.
    #[cfg(test)]
    pub(crate) fn foreign_latch(&self) -> ForeignLatch {
        Rc::clone(&self.gc.foreign)
    }

    /// The unscanned-root valve's **reused** scratch (its worklist and its visited set).
    ///
    /// One instance lives here for the runtime's whole lifetime, so no walk allocates: see
    /// [`WalkScratch`](crate::context::gc_walk::WalkScratch).
    pub(crate) fn walk_scratch(&mut self) -> &mut WalkScratch {
        &mut self.gc.walk
    }

    /// Moves the valve's **reused** walk scratch *out* of the runtime — the delivery seam's
    /// disjoint-borrow door.
    ///
    /// The unscanned-root valve classifies a word against the *receiver's* own collector, so the
    /// delivery seam ([`crate::context`]) must hold a `&Process` and the walk's scratch at the
    /// same time. The receiver is reached through a process table that may be the runtime's own
    /// (`runtime.processes`), and Rust cannot see that `ProcessTable` and `GcState` are disjoint
    /// through a `&mut Runtime`. Moving the scratch out for the duration sidesteps that: the
    /// swap is `O(1)` ([`std::mem::take`] leaves the empty default behind), the caller owns the
    /// scratch while it walks, and [`Runtime::put_walk_scratch`] hands it back unchanged — so the
    /// reuse (and the "a walk allocates nothing after warm-up" property) is preserved.
    pub(crate) fn take_walk_scratch(&mut self) -> WalkScratch {
        std::mem::take(&mut self.gc.walk)
    }

    /// Puts back the scratch [`Runtime::take_walk_scratch`] moved out.
    pub(crate) fn put_walk_scratch(&mut self, scratch: WalkScratch) {
        self.gc.walk = scratch;
    }

    /// Records that `pid`'s collection failed, and stops collecting for it.
    ///
    /// The pre-GC runtime was an unbounded arena that could not fail at the heap-admission
    /// point, so a failing collection must not become a program failure: the process simply
    /// keeps its (unmoved) heap and allocation continues to fall back to the arena.
    pub(crate) fn halt_process_gc(&mut self, pid: ProcessId) {
        self.gc.errors += 1;
        self.gc.halted.insert(pid);
    }

    /// Trips the global valve: no process heap is ever moved again.
    ///
    /// The first reason wins, so the diagnostic names the store that disabled collection.
    pub(crate) fn disable_gc(&mut self, reason: &'static str) {
        if !self.gc.disabled {
            self.gc.disabled = true;
            self.gc.disabled_reason = Some(reason);
        }
        self.gc.valve_trips += 1;
    }

    /// Why the global valve tripped, if it did.
    #[allow(dead_code)]
    pub(crate) fn gc_disabled_reason(&self) -> Option<&'static str> {
        self.gc.disabled_reason
    }

    /// Whether `name`'s module may be collected over — `true` when the module contains no
    /// float-family instruction (cached per module name).
    pub(crate) fn module_is_float_free(&mut self, name: &str, module: &Module) -> bool {
        if let Some(&known) = self.gc.float_free.get(name) {
            return known;
        }
        let answer = !crate::context::gc::module_uses_floats(module);
        self.gc.float_free.insert(name.to_string(), answer);
        answer
    }

    /// The **cached** answer to "may this module be collected over?", if the module has
    /// already been scanned.
    ///
    /// [`Runtime::module_is_float_free`] is the caching miss path (it needs the loaded
    /// module); this is the pure read [`crate::context::gc`] takes first, so the float scan
    /// runs at most once per module name for the runtime's lifetime.
    pub(crate) fn module_float_free_cached(&self, name: &str) -> Option<bool> {
        self.gc.float_free.get(name).copied()
    }
    /// The **escape-store** stabilization, with its two failure classes kept apart: copies
    /// `term` onto the pointer-stable arena and returns the copy.
    ///
    /// This is exactly [`Runtime::try_stabilize_escaped_with`] against an **empty** off-heap
    /// source table; a caller that holds the machine which built `term` uses that sibling, so an
    /// engine-built off-heap binary is resolved instead of tripping the valve.
    ///
    /// A term on a scheduled process's young heap is only kept alive while it is reachable
    /// from that process's root set ([`Process::collect`](beam_process::Process::collect)), so
    /// a term parked in a *runtime*-level store (a timer's message, a pending spawn's entry or
    /// arguments, a trace match spec, a persistent term, …) would otherwise dangle the moment
    /// the heap it lives on is moved or dropped. The copy is semantically identical (terms are
    /// immutable and compared structurally), so it is unobservable — for every term this
    /// runtime's own copy engine can actually copy.
    ///
    /// That copy ([`Runtime::stabilize_term`], i.e. `beam_message::copy_struct_simple`) can fail
    /// in two very different ways, and conflating them is exactly what this method exists to
    /// prevent:
    ///
    /// * **a genuine failure** — [`BeamError::SystemLimit`]. The term simply does not fit the
    ///   arena: [`crate::error::enforce_term_heap_limit`] translates the copy engine's
    ///   destination-heap exhaustion into the emulator's documented `error:system_limit` — the
    ///   preflight refusal at `beam_message/src/copy.rs:484-487` (the message-heap
    ///   `AllocFailure` built by `beam_message/src/error.rs:91-96`, recognised by
    ///   `beam_message::is_message_heap_exhaustion`, `beam_message/src/error.rs:82-84`) or a
    ///   mid-copy `Heap::alloc_words` failure (`copy.rs:396`/`448`). It is propagated as the
    ///   **honest error**: never stored anyway, and never a reason to freeze the node's
    ///   collection.
    /// * **the documented capability gap** — `copy_struct_simple` copies against **empty**
    ///   source/destination off-heap tables (`copy.rs:502-507`), and the copy engine can only
    ///   copy what it covers. A term carrying an **off-heap payload** — the 8-word
    ///   `[BinRef][ErlSubBits]` refc-binary pair, or an MSO container — is resolved through the
    ///   *source* off-heap table at `copy.rs:348-351` and `copy.rs:410-415`. A **refc binary**
    ///   whose lookup misses on an empty table is not refused: the copier **flattens** its window
    ///   into a fresh on-heap bitstring (`copy_match_context`), a byte-faithful self-contained
    ///   copy ([`Runtime::stabilize_term`]). What remains refused with
    ///   [`BeamError::BadArg`] (`crate::error::bad_term`,
    ///   `beam_message/src/error.rs:100-102`) is a term carrying an unresolvable **MSO
    ///   container** — as is the cycle guard's refusal of a forged
    ///   cyclic graph (`copy.rs:318-328`). So the split is exactly: **`Err(BeamError::SystemLimit)`
    ///   propagates, every other error is the capability gap** (that error is the *only* genuine
    ///   one `enforce_term_heap_limit` produces; `src/error.rs:89-97`). For it the original term
    ///   is kept — exactly as correct as before the collector existed — **and every process heap
    ///   is frozen** ([`Runtime::disable_gc`]), so the stored term can never dangle. The price is
    ///   that nothing is collected for the rest of the run, which is the right trade against a
    ///   wrong result.
    ///
    /// # Errors
    ///
    /// [`BeamError::SystemLimit`] when the term genuinely cannot be copied onto the arena. The
    /// valve is **not** tripped in that case.
    pub(crate) fn try_stabilize_escaped(&mut self, term: Eterm) -> Result<Eterm> {
        self.try_stabilize_escaped_with(term, &SourcePayloads::default())
    }

    /// [`Runtime::try_stabilize_escaped`] for a term that may reference the off-heap payload
    /// handles an engine machine built or the mailbox retained from a received message, resolved
    /// against `src`.
    ///
    /// The escape-store stabilization itself is unchanged (same two failure classes, same valve
    /// trip); only the copy's **source** off-heap table differs — it is built from `src` instead
    /// of being empty
    /// ([`Runtime::stabilize_term_with`](crate::runtime::Runtime::stabilize_term_with), the
    /// documented empty-source form unchanged). A caller that holds the handles behind
    /// `term` (the runner's exit-reason arm) therefore **shares** a term carrying an engine-built
    /// off-heap binary — or a received off-heap payload — instead of materializing it on-heap the
    /// way the empty-source form must, and neither form trips the valve over such a payload.
    ///
    /// # Errors
    ///
    /// As [`Runtime::try_stabilize_escaped`]: [`BeamError::SystemLimit`] when the term genuinely
    /// cannot be copied onto the arena (the valve is **not** tripped then).
    pub(crate) fn try_stabilize_escaped_with(
        &mut self,
        term: Eterm,
        src: &SourcePayloads,
    ) -> Result<Eterm> {
        match self.stabilize_term_with(term, src) {
            Ok(copy) => Ok(copy),
            // A genuine failure: the term does not fit the arena. Report it honestly — never
            // store the un-copied term, never freeze collection over it.
            Err(BeamError::SystemLimit) => Err(BeamError::SystemLimit),
            // The documented capability gap (an off-heap payload this crate cannot duplicate,
            // or a malformed graph): keep the original term and freeze every process heap.
            Err(_) => {
                self.disable_gc("an out-of-band store holds a term with an off-heap payload");
                Ok(term)
            }
        }
    }

    /// The reusable placeholder fill of `n` words the safe heap bump pads with.
    ///
    /// Only ever grown, so a bump never allocates (see
    /// [`crate::context::gc::bump`]); the words are [`NIL`](beam_term::NIL) — exactly what an
    /// uninitialized word of the arena is, so a block the engine allocated but has not written
    /// yet holds the same words on either heap.
    pub(crate) fn gc_scratch(&mut self, n: usize) -> &[Eterm] {
        if self.gc.scratch.len() < n {
            self.gc.scratch.resize(n, beam_term::NIL);
        }
        &self.gc.scratch[..n]
    }
}

// The per-pid latch's **probe accounting** for this thread: what a unit test can pin about the
// hot structure without a clock (a probe is a hash lookup and an insert is a hash lookup plus —
// only for a fresh pid — a growth, so "which probes happened" is the structural fact).
//
// Like `RUN_SLICE_COUNTERS` (`crate::runner`) and the valve counters
// (`crate::context::gc::valve_counters`) this is a `thread_local!`, never a shared static, so a
// concurrently running unit test's latch traffic cannot perturb this test's counts. The whole
// module is `#[cfg(test)]`: a release build has no counters, and the production accessors bind
// the `insert` verdict away instead of reading it (`#[cfg_attr(not(test), allow(
// unused_variables))]`), so nothing below costs the runtime anything.
#[cfg(test)]
pub(crate) mod latch_counters {
    use std::cell::Cell;

    /// One thread's latch traffic since the last [`reset`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct LatchCounters {
        /// `contains` probes that really **reached the set**: the empty-set short circuit in
        /// [`Runtime::foreign_reachable`](super::Runtime::foreign_reachable) answers `false`
        /// without a probe, so it is deliberately not counted (that is the point of the
        /// short circuit).
        pub(crate) probes: usize,
        /// `insert` calls — every call, including a re-insert of a pid the latch already holds
        /// (the valve's decline path inserts on every decline:
        /// [`Runtime::note_foreign_reachable`](super::Runtime::note_foreign_reachable) and
        /// [`Runtime::note_gc_fallback`](super::Runtime::note_gc_fallback)).
        pub(crate) inserts: usize,
        /// Of those, the calls that **grew** the latch (the pid was not already a member) — so
        /// `fresh_inserts` counts distinct pids and `inserts - fresh_inserts` counts the
        /// re-probes a still-open bit absorbs.
        pub(crate) fresh_inserts: usize,
        /// `remove` calls that **closed** an open bit
        /// ([`Runtime::close_foreign_reachable`](super::Runtime::close_foreign_reachable)) —
        /// i.e. how often a clean full-root descent retired a stale latch entry.
        pub(crate) removes: usize,
    }

    impl LatchCounters {
        /// The all-zero tally (the `const` form the thread-local's initialiser needs).
        const ZERO: LatchCounters = LatchCounters {
            probes: 0,
            inserts: 0,
            fresh_inserts: 0,
            removes: 0,
        };
    }

    thread_local! {
        /// This thread's latch traffic since the last [`reset`].
        static COUNTERS: Cell<LatchCounters> = const { Cell::new(LatchCounters::ZERO) };
    }

    /// This thread's latch counters since the last [`reset`].
    pub(crate) fn snapshot() -> LatchCounters {
        COUNTERS.with(Cell::get)
    }

    /// Zeroes this thread's latch counters (a measurement's "before" reading).
    pub(crate) fn reset() {
        COUNTERS.with(|counters| counters.set(LatchCounters::ZERO));
    }

    /// Records one `contains` probe of a **non-empty** latch.
    pub(crate) fn note_probe() {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.probes += 1;
            counters.set(value);
        });
    }

    /// Records one `insert` call, `fresh` being the set's own verdict (whether it grew).
    pub(crate) fn note_insert(fresh: bool) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.inserts += 1;
            value.fresh_inserts += usize::from(fresh);
            counters.set(value);
        });
    }

    /// Records one `remove` call, `closed` being the set's own verdict (whether the bit was open).
    pub(crate) fn note_remove(closed: bool) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.removes += usize::from(closed);
            counters.set(value);
        });
    }
}

#[cfg(test)]
mod tests {
    use beam_common::BeamError;
    use beam_engine::terms;
    use beam_engine::ExecutionContext;
    use beam_term::{compare, make_small, signed_value, Eterm};

    use super::*;
    use crate::config::RuntimeConfig;
    use crate::runtime::{PendingSpawn, MIN_HEAP_WORDS};
    use crate::runner::ProcessEntry;

    /// A runtime whose term-construction heaps — including the `-eval` heap the escape-store
    /// stabilization copies onto — are at the floor, so a term too large for the destination is
    /// cheap to build. The same rig `runtime.rs`'s
    /// `an_oversized_return_value_becomes_the_documented_system_limit` uses.
    fn small_runtime() -> Runtime {
        Runtime::new(RuntimeConfig::new().with_heap_words(MIN_HEAP_WORDS))
            .expect("the small-heap runtime")
    }

    /// A list far larger than the small runtime's destination heap (four cons words per element
    /// against a [`MIN_HEAP_WORDS`]-word heap), built on the unbounded engine arena.
    fn oversized_term(rt: &mut Runtime) -> Eterm {
        let elems: Vec<Eterm> = (0..(MIN_HEAP_WORDS as i64 * 4)).map(make_small).collect();
        let big = terms::make_list(&mut rt.arena, &elems).expect("the engine arena is unbounded");
        assert!(
            beam_term::size_object(big) > MIN_HEAP_WORDS,
            "the term must exceed the destination heap to exercise the split"
        );
        big
    }

    /// **The forced-GC latch is served, once.** A request recorded for a live process is
    /// consumed by [`Runtime::serve_gc_request`], which really collects it: the collection
    /// counter moves and no error/halt is recorded.
    #[test]
    fn a_forced_gc_request_is_served_exactly_once() {
        let mut rt = small_runtime();
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));
        let pid = table.spawn_default().expect("a process");

        assert!(!rt.gc_request_pending(pid), "nothing requested yet");
        rt.request_gc(pid);
        rt.request_gc(pid); // one bit per process: asking twice still collects once
        assert!(rt.gc_request_pending(pid));

        assert!(rt.serve_gc_request(&mut table, pid), "the request is served");
        assert_eq!(rt.gc_stats().collections, 1, "a real collection ran");
        assert_eq!(rt.gc_stats().errors, 0, "and it did not fail");
        assert!(!rt.gc_request_pending(pid), "the request was consumed");
        assert!(
            !rt.serve_gc_request(&mut table, pid),
            "there is nothing left to serve"
        );
        assert_eq!(rt.gc_stats().collections, 1, "and no second collection");
    }

    /// `erlang:statistics(garbage_collection)` read through the **real dispatch** (the
    /// [`RuntimeHooksBridge`](crate::context) arm), as `(GCs, WordsReclaimed, Reserved)`.
    fn statistics_garbage_collection(rt: &mut Runtime) -> (i64, i64, i64) {
        let key = rt.intern("garbage_collection");
        let mut ctx = RuntimeContext::new(rt);
        let value = ctx
            .call_bif("erlang", "statistics", 1, &[key])
            .expect("statistics(garbage_collection) is a served key");
        assert_eq!(terms::tuple_arity(value).expect("a 3-tuple"), 3);
        let element = |i: usize| terms::tuple_element(value, i).expect("a tuple element");
        (
            signed_value(element(0)),
            signed_value(element(1)),
            signed_value(element(2)),
        )
    }

    /// **`statistics(garbage_collection)` reads this runtime's own node-global GC counters.**
    /// A fresh runtime has run no collection at all — the C's own `{0, 0, 0}` shape — and a
    /// collection that really ran through [`Runtime::serve_gc_request`] moves **both** served
    /// figures by its own outcome: the collection count and the words that collection really
    /// reclaimed (never a placeholder, and never read from the live processes' collectors, which
    /// are empty of collections precisely when the workload's collections are the interesting
    /// ones).
    ///
    /// No oracle comparison here: the crate's test strategy forbids comparing live GC counters
    /// against the oracle, so every figure below is this runtime's own.
    #[test]
    fn statistics_garbage_collection_reads_the_node_global_collection_counters() {
        let mut rt = small_runtime();
        assert_eq!(
            statistics_garbage_collection(&mut rt),
            (0, 0, 0),
            "a fresh runtime has collected nothing, so all three elements are real zeros"
        );

        // A real collection through the forced-GC latch: the process's young heap carries an
        // unrooted (garbage) tuple, which the collection frees rather than copies.
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));
        let pid = table.spawn_default().expect("a process");
        table
            .get_mut(pid)
            .expect("the process is live")
            .heap_mut()
            .tuple(&[make_small(1), make_small(2), make_small(3)])
            .expect("a scratch tuple on the young heap");

        rt.request_gc(pid);
        assert!(rt.serve_gc_request(&mut table, pid), "the request is served");
        let stats = rt.gc_stats();
        assert_eq!(stats.collections, 1, "exactly the collection we forced");
        assert_eq!(stats.errors, 0, "and it did not fail");
        assert!(
            stats.reclaimed > 0,
            "the unrooted tuple really was reclaimed: {}",
            stats.reclaimed
        );

        let (gcs, words, reserved) = statistics_garbage_collection(&mut rt);
        assert_eq!(
            gcs,
            i64::try_from(stats.collections).expect("a small collection count"),
            "the runtime's own cumulative collection count"
        );
        assert_eq!(
            words,
            i64::try_from(stats.reclaimed).expect("a small reclaimed-word count"),
            "the words those collections really reclaimed"
        );
        assert_eq!(reserved, 0, "the C's reserved third element");
    }

    /// **A valve that forbids the move is never overridden by a request.** A pinned pid's and
    /// a globally-frozen pid's requests are consumed and answered `false`: the counters stay
    /// put, nothing is halted (a refusal is not a failure) and no collection is faked.
    #[test]
    fn a_forbidden_collection_is_refused_without_pretending() {
        let mut rt = small_runtime();
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));

        let pinned = table.spawn_default().expect("a process");
        // The pin set has no `Runtime` setter: the only production pin site (the runner's
        // float-register reaction) does not exist, so the valve is driven through its own field.
        rt.gc.pinned.insert(pinned);
        rt.request_gc(pinned);
        assert!(!rt.serve_gc_request(&mut table, pinned), "a pinned pid stays put");
        assert!(!rt.gc_request_pending(pinned), "the request is consumed either way");

        let frozen = table.spawn_default().expect("a second process");
        rt.disable_gc("test: the global valve");
        rt.request_gc(frozen);
        assert!(!rt.serve_gc_request(&mut table, frozen), "the valve is absolute");

        let stats = rt.gc_stats();
        assert_eq!(stats.collections, 0, "no collection was pretended");
        assert_eq!(stats.errors, 0, "a refusal is not a failure");
        assert_eq!(stats.halted, 0, "and it halts nothing");
    }

    /// **A request whose process is gone is pruned, a live one is kept.** Without the prune
    /// the request set would grow by one entry for every process that asked and then
    /// terminated before a collection point was reached.
    #[test]
    fn requests_of_dead_processes_are_pruned_and_live_ones_kept() {
        let mut rt = small_runtime();
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));
        let live = table.spawn_default().expect("a process");
        // A process that terminated with a request still pending: its slot is freed and its
        // serial bumped, so the table no longer holds the pid. (A fresh table's *first* spawn
        // would be number 0/serial 0 — the very pid `rt.self_pid()` names — so it cannot model
        // an absent process; a second, then-taken spawn does.)
        let gone = table.spawn_default().expect("a second, terminated process");
        table.take(gone).expect("the terminated process");

        rt.request_gc(live);
        rt.request_gc(gone);
        rt.prune_gc_requests(&table);
        assert!(rt.gc_request_pending(live), "a live process keeps its request");
        assert!(
            !rt.gc_request_pending(gone),
            "a pid the table does not hold is pruned"
        );
    }

    /// **The async completion is emitted, with the oracle's shape and its liveness result.** A
    /// recorded `{async, ReqId}` request owes its requester `{garbage_collect, ReqId, true}` for
    /// a **live** target and `{garbage_collect, ReqId, false}` for one the table no longer holds
    /// — the C's `notify_sys_task_executed` (`erl_process.c:10574-10623`) over
    /// `liveness_notify_sys_task_executed`'s two results (`erl_process.c:11488-11490`). Only a
    /// **live** target has the forced-GC latch armed, and a requester that is itself gone is not
    /// notified at all.
    ///
    /// Oracle (`erl -noshell -eval '…'`, OTP 29.1.1 / erts-17.1): a live blocked target prints
    /// `async` then `{garbage_collect, #Ref<…>, true}`; a terminated target prints `async` then
    /// `{garbage_collect, #Ref<…>, false}`.
    #[test]
    fn an_async_garbage_collect_completion_is_delivered_to_the_requester() {
        let mut rt = small_runtime();
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));
        let target = table.spawn_default().expect("a target process");
        let requester = table.spawn_default().expect("a requester process");
        let gone = table.spawn_default().expect("a target that then terminates");
        table.take(gone).expect("the terminated target");

        // The `ReqId` is any term, built here on the runtime's own (pointer-stable) arena.
        let elems = [make_small(1), make_small(2)];
        let req_id = terms::make_list(&mut rt.arena, &elems).expect("the arena term");

        rt.request_gc_async(target, requester, req_id, true);
        rt.request_gc_async(gone, requester, req_id, false);
        assert!(
            rt.gc_request_pending(target),
            "a live target's request is armed"
        );
        assert!(
            !rt.gc_request_pending(gone),
            "a target the table does not hold has nothing to collect"
        );

        assert_eq!(
            rt.deliver_gc_completions(&mut table).expect("both send"),
            2,
            "one completion per request"
        );
        // Drained: a second boundary has nothing left to re-report.
        assert_eq!(
            rt.deliver_gc_completions(&mut table).expect("nothing left"),
            0,
            "a discharged request is never reported twice"
        );

        let payload = |table: &ProcessTable, at: usize| -> Eterm {
            let process = table.get(requester).expect("the requester");
            let cursor = process.mailbox().cursor_at(at);
            process
                .mailbox()
                .message_at(cursor)
                .expect("a completion message")
                .payload()
        };
        let expected = |rt: &mut Runtime, result: &str| -> Eterm {
            let mut ctx = RuntimeContext::new(rt);
            let (tag, result) = {
                let atoms = ctx.runtime.code_server.atoms_mut();
                (
                    make_atom(atoms.intern("garbage_collect")),
                    make_atom(atoms.intern(result)),
                )
            };
            terms::make_tuple(&mut ctx, &[tag, req_id, result]).expect("the expected tuple")
        };
        assert!(
            compare::eq(payload(&table, 0), expected(&mut rt, "true"), rt.atoms()),
            "a live target's completion is `{{garbage_collect, ReqId, true}}`"
        );
        assert!(
            compare::eq(payload(&table, 1), expected(&mut rt, "false"), rt.atoms()),
            "a gone target's completion is `{{garbage_collect, ReqId, false}}`"
        );

        // A requester that has itself terminated has nobody to notify: the completion is
        // dropped, never delivered elsewhere and never reported as delivered.
        let orphan = table.spawn_default().expect("a third requester");
        table.take(orphan).expect("the requester terminates");
        rt.request_gc_async(target, orphan, req_id, true);
        assert_eq!(
            rt.deliver_gc_completions(&mut table).expect("nothing to deliver"),
            0,
            "a dead requester is not notified"
        );
        assert_eq!(
            table
                .get(requester)
                .expect("the requester")
                .mailbox()
                .len(),
            2,
            "and no message leaked into another process's mailbox"
        );
    }

    /// **The honest split for a genuine failure.** A term the copier cannot fit is reported as
    /// the emulator's documented `error:system_limit` — and, because that failure is *real*
    /// rather than a capability gap, the global valve stays closed: one un-copyable store must
    /// not freeze every process heap in the node.
    #[test]
    fn an_uncopyable_escaped_term_is_the_honest_system_limit() {
        let mut rt = small_runtime();
        let big = oversized_term(&mut rt);
        assert!(
            matches!(rt.try_stabilize_escaped(big), Err(BeamError::SystemLimit)),
            "a term the copier cannot fit is the documented system_limit"
        );
        let stats = rt.gc_stats();
        assert!(!stats.disabled, "a genuine failure must not freeze collection");
        assert_eq!(stats.valve_trips, 0, "a genuine failure trips no valve");
    }

    /// **The happy path.** A copyable term becomes the pointer-stable copy (structurally equal,
    /// different address) and the valve stays closed.
    #[test]
    fn a_copyable_escaped_term_is_copied_and_leaves_the_valve_closed() {
        let mut rt = small_runtime();
        let elems = [make_small(1), make_small(2), make_small(3)];
        let original = terms::make_list(&mut rt.arena, &elems).expect("the engine arena");
        let copy = rt.try_stabilize_escaped(original).expect("a small term copies");
        assert!(
            compare::eq(copy, original, rt.atoms()),
            "the copy must be structurally the original"
        );
        assert_ne!(copy, original, "the copy lives on the destination heap");
        let stats = rt.gc_stats();
        assert!(!stats.disabled, "a successful copy trips no valve");
        assert_eq!(stats.valve_trips, 0);
    }

    /// **The pending-spawn store propagates the honest failure.** A child whose entry fun or
    /// argument genuinely cannot be copied is recorded **nowhere** — the pass is handed the
    /// honest `system_limit` instead of a record holding a dangling term — and no valve trips.
    #[test]
    fn a_pending_spawn_that_cannot_be_stabilized_is_recorded_nowhere() {
        let mut rt = small_runtime();
        let fun = oversized_term(&mut rt);
        let spawn = PendingSpawn {
            pid: rt.self_pid(),
            entry: ProcessEntry::Fun { fun, target: None },
            args: Vec::new(),
        };
        assert!(
            matches!(rt.note_pending_spawn(spawn), Err(BeamError::SystemLimit)),
            "an un-copyable entry is reported honestly"
        );
        assert!(rt.pending_spawns.is_empty(), "nothing was recorded");
        let stats = rt.gc_stats();
        assert!(!stats.disabled, "an honest failure trips no valve");
        assert_eq!(stats.valve_trips, 0);
    }

    /// The counterpart: a recordable spawn is still recorded, with both the entry fun and the
    /// arguments **copied** onto the arena.
    #[test]
    fn a_stabilizable_pending_spawn_is_recorded_with_arena_copies() {
        let mut rt = small_runtime();
        let fun = make_small(7);
        let elems = [make_small(1), make_small(2)];
        let arg = terms::make_list(&mut rt.arena, &elems).expect("the engine arena");
        rt.note_pending_spawn(PendingSpawn {
            pid: rt.self_pid(),
            entry: ProcessEntry::Fun { fun, target: None },
            args: vec![arg],
        })
        .expect("a recordable spawn");
        assert_eq!(rt.pending_spawns.len(), 1, "the record was kept");
        let recorded = &rt.pending_spawns[0];
        match &recorded.entry {
            ProcessEntry::Fun {
                fun: recorded_fun, ..
            } => assert_eq!(*recorded_fun, fun),
            ProcessEntry::Export { .. } => panic!("a fun entry was recorded"),
        }
        let copied = recorded.args[0];
        assert!(
            compare::eq(copied, arg, rt.atoms()),
            "the argument is the arena copy"
        );
        assert_ne!(copied, arg, "the copy lives on the destination heap");
        assert!(!rt.gc_stats().disabled, "a successful store trips no valve");
    }

    /// **The valve accessors answer identically whether their set is empty or populated** — the
    /// empty-set short circuit only *skips* the (SipHash) probe, it never changes a verdict.
    ///
    /// [`Runtime::gc_process_enabled`] and [`Runtime::gc_request_pending`] run on **every**
    /// engine heap-admission point ([`crate::context::gc::ensure_heap`]), *before* the
    /// occupancy short-circuit, and [`Runtime::foreign_reachable`] gates the descent on the
    /// delivery path; in a normal workload all four sets (pinned, halted, requested, foreign)
    /// are empty, so each verdict must cost a length compare instead of a hash that can only
    /// miss. Every verdict below is pinned for the empty state, for the populated state, and
    /// for an unaffected neighbouring pid.
    #[test]
    fn the_valve_accessors_answer_the_same_for_an_empty_and_a_populated_set() {
        let mut rt = small_runtime();
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));
        let a = table.spawn_default().expect("a process");
        let b = table.spawn_default().expect("another process");

        // -- the empty state: every set is empty, so every accessor short-circuits --
        assert!(rt.gc_enabled(), "a fresh runtime's global valve is closed");
        assert!(rt.gc_process_enabled(a), "nothing pins or halts a fresh pid");
        assert!(rt.gc_process_enabled(b), "and another pid is enabled too");
        assert!(!rt.gc_request_pending(a), "no request is pending");
        assert!(!rt.gc_request_pending(b), "nor for another pid");
        assert!(!rt.foreign_reachable(a), "no descent bit is set");
        assert!(!rt.foreign_reachable(b), "nor for another pid");

        // -- `requested`: a populated set still answers per-pid .. --
        rt.request_gc(a);
        assert!(rt.gc_request_pending(a), "a's request is now pending");
        assert!(!rt.gc_request_pending(b), "b's is not");
        assert!(rt.take_gc_request(a), "the request is consumed");
        assert!(!rt.take_gc_request(a), "and there was only one");
        assert!(!rt.gc_request_pending(a), "so the set is empty again");

        // -- .. and the `foreign` descent bit is per-pid and add-only --
        rt.note_foreign_reachable(a);
        assert!(rt.foreign_reachable(a), "a's descent gate is open");
        assert!(!rt.foreign_reachable(b), "b's is untouched");

        // -- `halted`: reached through the production halt site --
        rt.halt_process_gc(b);
        assert!(!rt.gc_process_enabled(b), "a halted pid stays put");
        assert!(
            rt.gc_process_enabled(a),
            "halting one pid does not halt another"
        );
        assert_eq!(rt.gc_stats().halted, 1, "the halt is recorded");

        // -- `pinned`: no `Runtime` setter exists (as
        // `a_forbidden_collection_is_refused_without_pretending` also establishes), so the
        // valve is driven through its own field --
        rt.gc.pinned.insert(a);
        assert!(!rt.gc_process_enabled(a), "a pinned pid stays put");
        assert!(
            rt.gc_enabled(),
            "a per-pid valve never closes the global one"
        );

        // None of the above collected, halted or faked anything beyond what it recorded.
        let stats = rt.gc_stats();
        assert_eq!(stats.collections, 0, "no collection was pretended");
        assert_eq!(
            stats.errors, 1,
            "exactly the one recorded halt (its site counts the failure)"
        );
        assert_eq!(stats.halted, 1, "exactly the one halt above");
        assert_eq!(stats.pinned, 1, "exactly the one pin above");
    }

    /// The latch's membership corpus: real table pids (this runtime's dense internal ids) plus
    /// boundary-shaped synthetic ones, so the equivalence below is not checked over a single key
    /// shape — the same argument the `hasher` module's own corpus test makes for the hasher.
    fn latch_corpus(table: &mut ProcessTable) -> Vec<ProcessId> {
        let mut corpus: Vec<ProcessId> = (0..6)
            .map(|_| table.spawn_default().expect("a corpus process"))
            .collect();
        corpus.push(ProcessId::new(0, 0));
        corpus.push(ProcessId::new(u64::MAX, u64::MAX));
        corpus.push(ProcessId::new(1 << 32, u64::from(u32::MAX)));
        corpus
    }

    /// **The latch is hasher-independent.** Every membership verdict the runtime's **one shared**
    /// foreign-reachability cell answers — through the production accessors, and read back through
    /// [`Runtime::foreign_latch`], the cell the installed
    /// [`RuntimeCollectionGate`](crate::context::gc_walk::RuntimeCollectionGate) also holds a clone
    /// of — is exactly the verdict a `RandomState`-backed `HashSet` over the same pids answers, for
    /// a mixed corpus of present and absent keys.
    ///
    /// This is the soundness witness for the latch's own [`crate::hasher`]: a set-membership
    /// answer does not depend on the hash function (equal keys hash equally, colliding keys are
    /// separated by the table's equality probe), so the cheaper hasher may change the probe *cost*
    /// and the (unobservable) iteration order — and nothing else.
    #[test]
    fn the_latch_answers_membership_exactly_as_a_default_hasher_set() {
        let mut rt = small_runtime();
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));
        let corpus = latch_corpus(&mut table);
        let mut reference: HashSet<ProcessId> = HashSet::new();
        for (index, pid) in corpus.iter().enumerate() {
            // A mixed pattern: every third pid is left unrecorded, so both verdicts are exercised
            // for keys that are and are not members.
            if index % 3 != 2 {
                rt.note_foreign_reachable(*pid);
                reference.insert(*pid);
            }
            assert_eq!(
                rt.foreign_reachable(*pid),
                reference.contains(pid),
                "the membership verdict agrees with the default-hasher set for {pid:?}"
            );
            assert_eq!(
                rt.foreign_latch().borrow().len(),
                reference.len(),
                "the latch holds exactly the same keys as the default-hasher set"
            );
        }
        // A pid in neither set, and the read-back through the shared cell's own handle.
        let absent = ProcessId::new(7, 7);
        assert!(!rt.foreign_reachable(absent), "a pid in neither set misses");
        assert!(!reference.contains(&absent));
        assert!(
            rt.foreign_latch().borrow().contains(&corpus[0]),
            "the cell `foreign_latch` hands out is the one the accessors wrote"
        );
        // The latch holds the reference set's keys — sorted, because the iteration ORDER is the
        // one thing a hasher change may alter, and nothing in this crate observes it.
        let mut from_latch: Vec<ProcessId> = rt.foreign_latch().borrow().iter().copied().collect();
        let mut from_reference: Vec<ProcessId> = reference.iter().copied().collect();
        from_latch.sort();
        from_reference.sort();
        assert_eq!(from_latch, from_reference, "the same keys, whatever the order");
        assert!(
            !rt.gc_stats().disabled,
            "nothing above was a collection or a valve trip"
        );
    }

    /// **The latch's traffic is structural, never a clock.** An empty latch is not probed at all
    /// (the accessor's short circuit), insert-then-lookup hits, a re-insert does **not** grow the
    /// set, and the thread-local counters see one insert per call with exactly one *fresh* insert
    /// per distinct pid — including through the arena-fallback door, which opens the same gate.
    #[test]
    fn the_latch_grows_only_on_a_fresh_pid_and_counts_every_probe() {
        let mut rt = small_runtime();
        let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
            &mut beam_term::AtomTable::new(),
        ));
        let mut corpus = latch_corpus(&mut table).into_iter();
        let a = corpus.next().expect("a corpus pid");
        let b = corpus.next().expect("another corpus pid");
        let c = corpus.next().expect("a third corpus pid");

        latch_counters::reset();
        // --- (b) an empty latch costs no probe, and the inserted pid then hits ---
        assert!(!rt.foreign_reachable(a), "nothing is recorded yet");
        assert_eq!(
            latch_counters::snapshot().probes,
            0,
            "an empty latch is answered by the length compare, never by a probe"
        );
        rt.note_foreign_reachable(a);
        assert!(rt.foreign_reachable(a), "insert-then-lookup hits");
        assert!(!rt.foreign_reachable(b), "an unrecorded pid still misses");
        assert_eq!(
            latch_counters::snapshot().probes,
            2,
            "both lookups above probed the now-populated latch"
        );
        assert_eq!(latch_counters::snapshot().inserts, 1, "one insert so far");
        // --- (c) re-inserting the same pid does not grow the set ---
        rt.note_foreign_reachable(a);
        rt.note_foreign_reachable(a);
        assert_eq!(
            rt.foreign_latch().borrow().len(),
            1,
            "the latch still holds exactly one pid"
        );
        assert_eq!(latch_counters::snapshot().inserts, 3, "three insert calls");
        assert_eq!(
            latch_counters::snapshot().fresh_inserts,
            1,
            "but only the first one grew the latch"
        );
        // --- (d) one fresh insert per distinct pid, however often it is re-inserted ---
        rt.note_foreign_reachable(b);
        rt.note_foreign_reachable(c);
        rt.note_foreign_reachable(b);
        assert_eq!(
            rt.foreign_latch().borrow().len(),
            3,
            "three distinct pids are latched"
        );
        let counters = latch_counters::snapshot();
        assert_eq!(counters.inserts, 6, "one insert per call");
        assert_eq!(
            counters.fresh_inserts, 3,
            "one FRESH insert per distinct pid, and no more"
        );
        assert_eq!(
            counters.probes, 2,
            "only the lookups above probed the latch"
        );
        // --- the arena-fallback door opens the very same gate, and counts the same way ---
        rt.note_gc_fallback(c);
        assert_eq!(
            rt.foreign_latch().borrow().len(),
            3,
            "an already-latched pid does not grow the set"
        );
        assert_eq!(rt.gc_stats().fallbacks, 1, "the fallback door really ran");
        let counters = latch_counters::snapshot();
        assert_eq!(
            counters.inserts, 7,
            "the fallback's insert is counted like any other"
        );
        assert_eq!(counters.fresh_inserts, 3, "and it was not fresh");
        // Nothing above collected, and no verdict pretended otherwise.
        assert_eq!(rt.gc_stats().collections, 0, "no collection was pretended");
    }
}
