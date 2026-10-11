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
//! Everything here is counters plus two small hash sets, deliberately: the runtime is
//! single-threaded and these are read on the dispatch path, not the hot path (the GC point
//! is only armed inside `runner::budget::BudgetContext::ensure_heap`).
//!
//! # Wiring
//!
//! The state is owned by [`Runtime`] (a `gc: GcState` field), the collection point is
//! [`crate::context::gc::ensure_heap`] (armed by the slice's own
//! [`BudgetContext::ensure_heap`](crate::runner)), and every escape store — the runner's
//! exit-reason stabilization included — goes through [`Runtime::try_stabilize_escaped`]. See
//! those call sites for the details.
use std::collections::{HashMap, HashSet};

use beam_common::{BeamError, Result};
use beam_gc::CollectionKind;
use beam_loader::Module;
use beam_process::{ProcessId, ProcessTable};
use beam_term::Eterm;

use super::{Runtime, SourcePayloads};

/// A snapshot of this runtime's process-heap collection activity.
///
/// The counters are **measured own-state** figures (they describe this runtime's own
/// collector, not the oracle's) and exist so a test — or a future `statistics/1` — can see
/// that the collector really ran, and *why it did not* when it did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcStatsSnapshot {
    /// How many collections ran (minor **and** major).
    pub collections: u64,
    /// How many of [`collections`](GcStatsSnapshot::collections) were fullsweeps.
    pub major: u64,
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
            disabled: false,
            disabled_reason: None,
            valve_trips: 0,
            collections: 0,
            major: 0,
            fallbacks: 0,
            errors: 0,
            scratch: Vec::new(),
        }
    }
}

// The wiring lands the call sites incrementally (the collection point first, then the
// escape-store stabilization), so a helper here whose caller has not arrived yet is
// legitimately unused for now — `gc_disabled_reason` is the one such diagnostic.
impl Runtime {
    /// The runtime's collection counters and safety state.
    pub fn gc_stats(&self) -> GcStatsSnapshot {
        GcStatsSnapshot {
            collections: self.gc.collections,
            major: self.gc.major,
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
        self.gc_enabled() && !self.gc.pinned.contains(&pid) && !self.gc.halted.contains(&pid)
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

    /// Whether `pid` has an **unserved** forced-GC request (see [`Runtime::request_gc`]).
    pub(crate) fn gc_request_pending(&self, pid: ProcessId) -> bool {
        self.gc.requested.contains(&pid)
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
    /// This is a legitimate collection site: the caller has just published the parked
    /// machine's whole live root set into `pid`
    /// ([`publish_parked_roots`](crate::runner)), and the machine's float-register file is
    /// *provably* dead — the post-slice scan pins any pid whose `Machine::fr` holds a term,
    /// so a pid that is still [`gc_process_enabled`](Runtime::gc_process_enabled) cannot be
    /// carrying one. The collector therefore relocates exactly what is live, against the
    /// process's own root set.
    ///
    /// Returns whether a collection ran. The answer is `false` — with the counters
    /// untouched, never a fabricated collection — for a request the valves refuse (a pinned
    /// or halted pid, the global escape-store valve), for a pid the table no longer holds,
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
        match process.collect() {
            Ok(collection) => {
                self.note_gc_collection(matches!(collection.kind, CollectionKind::Major));
                true
            }
            Err(_) => {
                self.halt_process_gc(pid);
                false
            }
        }
    }

    /// Drops the forced-GC requests of processes `table` no longer holds.
    ///
    /// A process that terminated with a request still pending (it reached no heap-admission
    /// point and no park after asking) can never be served — its heap is gone — so the entry
    /// is noise that would otherwise accumulate for the runtime's whole lifetime, one per
    /// such process. The scheduler pass prunes at its boundary, where the pass's own table
    /// holds exactly the live processes.
    pub(crate) fn prune_gc_requests(&mut self, table: &ProcessTable) {
        if self.gc.requested.is_empty() {
            return;
        }
        self.gc.requested.retain(|pid| table.contains(*pid));
    }

    /// Pins `pid` on the runtime: its heap is never moved again.
    ///
    /// Used when the machine's float-register file holds a live term — `Machine::fr` is not
    /// part of the root set the engine hands `ensure_heap`, and this crate may neither read
    /// nor write it from there, so a move while a float is live could silently corrupt float
    /// code. Pinning is permanent for the runtime: the register is only cleared by
    /// `Machine::reset`, which this crate cannot observe.
    pub(crate) fn pin_process(&mut self, pid: ProcessId) {
        self.gc.pinned.insert(pid);
    }

    /// Records that a collection ran.
    pub(crate) fn note_gc_collection(&mut self, major: bool) {
        self.gc.collections += 1;
        if major {
            self.gc.major += 1;
        }
    }

    /// Records that an engine allocation could not be served from a process young heap and
    /// fell back to the pointer-stable arena.
    pub(crate) fn note_gc_fallback(&mut self) {
        self.gc.fallbacks += 1;
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
    ///   *source* off-heap table at `copy.rs:348-351` and `copy.rs:410-415`; the lookup misses
    ///   on an empty table and the copy is refused with
    ///   [`BeamError::BadArg`] (`crate::error::bad_term`,
    ///   `beam_message/src/error.rs:100-102`) — as is the cycle guard's refusal of a forged
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
    /// documented empty-source degradation unchanged). A caller that holds the handles behind
    /// `term` (the runner's exit-reason arm) therefore stabilizes a term carrying an engine-built
    /// off-heap binary — or a received off-heap payload — **correctly**, instead of tripping the
    /// valve over a payload the empty table simply could not resolve.
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

#[cfg(test)]
mod tests {
    use beam_common::BeamError;
    use beam_engine::terms;
    use beam_term::{compare, make_small, Eterm};

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
        rt.pin_process(pinned);
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
}
