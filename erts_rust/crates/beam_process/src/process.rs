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

//! The process control block: [`Process`], its state, flags, message delivery and GC entry.
//!
//! `Process` is the PCB (the legacy C `Process` struct, `erts/emulator/beam/erl_process.h`)
//! reduced to what the single-scheduler Milestone-1 core needs: identity, scheduler state
//! and flags, the collector-owned heap, the mailbox, the registers/stack root set, the
//! reduction counter, the link/monitor sets and the termination reason.
//!
//! Nothing here is reimplemented from a sibling: the mailbox is `beam_message::Queue`, the
//! copy engine is `beam_message::send`/`receive`, the heaps are `beam_gc::AnyCollector` over
//! `beam_alloc::Heap`, and the off-heap objects are `beam_message::OffHeapTable`.
//!
//! Legacy C ground truth (read-only): `erl_process.{c,h}` (the PCB and its state),
//! `erl_message.h` (`ERL_MESSAGE_*`) and `erl_gc.c` (`erts_garbage_collect`).

use beam_alloc::{Heap, RefcBinary};
use beam_common::{AllocKind, BeamError, Result};
use beam_gc::{AnyCollector, Collection, HeapPolicy, OffHeap, OffHeapEntry, ReachableObject};
use beam_message::{copy_struct, CopyContext, Message, OffHeapTable, Queue};
use beam_term::prelude::{Eterm, HEADER_BIN_REF};
use std::collections::{HashMap, HashSet};

use crate::dict::Dict;
use crate::error;
use crate::fragments::SavedArgs;
use crate::link::Links;
use crate::monitor::{MonitoredBy, Monitors, PortWatch};
use crate::pid::ProcessId;
use crate::registers::Registers;
use crate::run_queue::Priority;
use crate::signal_queue::SignalQueue;

// A test-only probe counting [`Process::owned_binary_slot`] resolutions — one unit of work per
// `data_ptr → slot` lookup, i.e. one per [`Process::register_off_heap_binary`] call.
//
// It is a **thread-local** cell so a test's count is deterministic even though cargo runs tests in
// parallel on separate threads: a registration another test performs on its own thread cannot
// appear in this thread's count. Kept `#[cfg(test)]` so it costs production nothing, and a
// thread-local rather than a `Process` field because the frozen field shape is constructible by
// struct literal in test builds too.
#[cfg(test)]
thread_local! {
    static REGISTRATION_PROBES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Bumps the test-only registration probe (see `REGISTRATION_PROBES` below): one unit per
/// `data_ptr → slot` resolution in [`Process::owned_binary_slot`].
#[cfg(test)]
fn probe_registration() {
    REGISTRATION_PROBES.with(|counter| counter.set(counter.get() + 1));
}

/// The calling thread's current registration-probe count (see `REGISTRATION_PROBES` below).
#[cfg(test)]
fn registration_probes() -> u64 {
    REGISTRATION_PROBES.with(std::cell::Cell::get)
}

/// The scheduler-visible state of a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProcessState {
    /// Ready to run; currently in a run queue.
    Runnable,
    /// Currently executing on a scheduler.
    Running,
    /// Blocked in `receive` (or another wait) until a message/signal arrives.
    Waiting,
    /// Suspended from outside (`erlang:suspend_process/1`); not schedulable.
    Suspended,
    /// Terminating: the exit reason is set and propagation is in progress.
    Exiting,
    /// Fully terminated; the PCB is waiting to be reclaimed.
    Terminated,
}

impl ProcessState {
    /// The atom `erlang:process_info(P, status)` reports for this state.
    ///
    /// This is the oracle's status set (`OTP 29.1.1` / `erts-17.1`): a process blocked in
    /// `receive` reports `waiting`, a freshly spawned process that has not run yet reports
    /// `runnable`, and a running process reports `running`. A **terminated** process has no
    /// status at all — the oracle's whole `process_info` call returns `undefined` for a dead
    /// pid — so [`ProcessState::Terminated`] maps to `None` and the BIF layer renders
    /// `undefined`.
    ///
    /// The transient `garbage_collecting` status is deliberately **not** produced here: it
    /// overrides the state *while a process is being collected*, so it is layered on top by
    /// [`Process::status`].
    pub fn status_atom(&self) -> Option<&'static str> {
        match self {
            ProcessState::Running => Some("running"),
            ProcessState::Runnable => Some("runnable"),
            ProcessState::Waiting => Some("waiting"),
            ProcessState::Suspended => Some("suspended"),
            ProcessState::Exiting => Some("exiting"),
            ProcessState::Terminated => None,
        }
    }
}

/// What a successful [`Process::deliver`] did with the recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliverOutcome {
    /// The message was enqueued; the recipient was not blocked, so nothing else happened.
    Enqueued,
    /// The message was enqueued **and** the recipient was blocked (`Waiting`/`Suspended`):
    /// it has been made `Runnable` so the scheduler can resume it.
    Woke,
}

impl DeliverOutcome {
    /// Whether the delivery woke a blocked recipient (i.e. this is [`DeliverOutcome::Woke`]).
    pub fn woke(self) -> bool {
        matches!(self, DeliverOutcome::Woke)
    }
}

/// The process's boolean flags, as a `u32` bitset newtype (no `bitflags` dependency).
///
/// These are the flags the Milestone-1 core reads directly: `trap_exit`, `system`,
/// `in_run_queue`, `garbage_collecting`, `active` and `sensitive`. The legacy C `F_*` word
/// is far wider; the rest is a follow-up.
pub struct ProcessFlags {
    bits: u32,
}

/// The `trap_exit` flag bit.
const FLAG_TRAP_EXIT: u32 = 1 << 0;
/// The `system` flag bit.
const FLAG_SYSTEM: u32 = 1 << 1;
/// The `in_run_queue` flag bit.
const FLAG_IN_RUN_QUEUE: u32 = 1 << 2;
/// The `garbage_collecting` flag bit.
const FLAG_GARBAGE_COLLECTING: u32 = 1 << 3;
/// The `active` flag bit.
const FLAG_ACTIVE: u32 = 1 << 4;
/// The `sensitive` flag bit (`process_flag(sensitive, Bool)`).
const FLAG_SENSITIVE: u32 = 1 << 5;

/// How many envelope words one queued message contributes to a collection's root slice.
///
/// The order is fixed: `[payload, from, token]` — see [`Process::run_rooted_collection`], which
/// gathers them in that order and scatters them back through
/// [`Message::with_payload`]/[`Message::set_from`]/[`Message::set_token`].
const MESSAGE_ROOT_WORDS: usize = 3;
/// The offset of a message's payload word within its [`MESSAGE_ROOT_WORDS`] slice.
const MESSAGE_ROOT_PAYLOAD: usize = 0;
/// The offset of a message's `from` word within its [`MESSAGE_ROOT_WORDS`] slice.
const MESSAGE_ROOT_FROM: usize = 1;
/// The offset of a message's `token` word within its [`MESSAGE_ROOT_WORDS`] slice.
const MESSAGE_ROOT_TOKEN: usize = 2;

/// The off-heap payload pointer a **reachable** object names, or `None` when the object is not an
/// off-heap binary handle.
///
/// This is the per-object test [`Process::sweep_off_heap_binaries_reachable`] applies to the
/// [`beam_gc::ReachableObject`] views [`beam_gc::AnyCollector::for_each_reachable_object_view`]
/// reports. An off-heap refc binary's on-heap handle is the frozen `[BinRef(3 words)][ErlSubBits(5
/// words)]` pair (`beam_term::refc`), and the payload pointer — the frozen `BinRef.val` contract,
/// identical to [`RefcBinary::data_ptr`] — is exactly `BinRef`'s word 1, read here through the
/// view's **safe** [`word`](ReachableObject::word) accessor (no raw-pointer arithmetic of our own).
///
/// The collector's walk descends an `ErlSubBits`'s `orig` slot unconditionally (the canonical copy
/// child walk of `beam_term::layout::for_each_child_slot`), so the `BinRef` behind **every**
/// reachable `ErlSubBits` is reported to the walk as an object of its own; testing the header word
/// alone therefore covers the whole refc pair, whichever of the two objects is reached first (and a
/// match-context `ErlSubBits`, whose `orig` is an on-heap bitstring, is correctly ignored).
fn reachable_bin_ref_payload(object: &ReachableObject<'_>) -> Option<usize> {
    if object.header() != HEADER_BIN_REF {
        return None;
    }
    // Word 1 of a `BinRef` is its `val` slot — the payload's first data byte. The view reports the
    // object's header plus that word, so the lookup is a plain safe read; a narrower object (a shape
    // whose header lied) simply has no word 1 and names nothing.
    object.word(1).map(|val| val.raw())
}

impl ProcessFlags {
    /// No flags set.
    pub fn new() -> ProcessFlags {
        ProcessFlags { bits: 0 }
    }

    /// The raw bitset.
    pub fn bits(&self) -> u32 {
        self.bits
    }

    /// Rebuilds the flags from a raw bitset.
    pub fn from_bits(bits: u32) -> ProcessFlags {
        ProcessFlags { bits }
    }

    /// Whether the process traps exit signals (`process_flag(trap_exit, true)`).
    pub fn trap_exit(&self) -> bool {
        self.get(FLAG_TRAP_EXIT)
    }

    /// Sets the `trap_exit` flag.
    pub fn set_trap_exit(&mut self, value: bool) {
        self.set(FLAG_TRAP_EXIT, value);
    }

    /// Whether the process belongs to a system/OS process (`system` flag).
    pub fn system(&self) -> bool {
        self.get(FLAG_SYSTEM)
    }

    /// Sets the `system` flag.
    pub fn set_system(&mut self, value: bool) {
        self.set(FLAG_SYSTEM, value);
    }

    /// Whether the process is currently enqueued in a run queue.
    pub fn in_run_queue(&self) -> bool {
        self.get(FLAG_IN_RUN_QUEUE)
    }

    /// Sets the `in_run_queue` flag.
    pub fn set_in_run_queue(&mut self, value: bool) {
        self.set(FLAG_IN_RUN_QUEUE, value);
    }

    /// Whether the process is currently being garbage collected.
    pub fn garbage_collecting(&self) -> bool {
        self.get(FLAG_GARBAGE_COLLECTING)
    }

    /// Sets the `garbage_collecting` flag.
    pub fn set_garbage_collecting(&mut self, value: bool) {
        self.set(FLAG_GARBAGE_COLLECTING, value);
    }

    /// Whether the process is active (schedulable at all).
    pub fn active(&self) -> bool {
        self.get(FLAG_ACTIVE)
    }

    /// Sets the `active` flag.
    pub fn set_active(&mut self, value: bool) {
        self.set(FLAG_ACTIVE, value);
    }

    /// Whether the process is `sensitive` (`process_flag(sensitive, true)`).
    ///
    /// A sensitive process uses a pre-allocated heap fragment for temporary allocations so
    /// that a failure can be reported without allocating more memory.
    pub fn sensitive(&self) -> bool {
        self.get(FLAG_SENSITIVE)
    }

    /// Sets the `sensitive` flag.
    pub fn set_sensitive(&mut self, value: bool) {
        self.set(FLAG_SENSITIVE, value);
    }

    /// Reads one flag bit.
    fn get(&self, bit: u32) -> bool {
        self.bits & bit != 0
    }

    /// Writes one flag bit.
    fn set(&mut self, bit: u32, value: bool) {
        if value {
            self.bits |= bit;
        } else {
            self.bits &= !bit;
        }
    }
}

/// The process control block (PCB).
///
/// Owns the process's identity, scheduler state, heap (via the collector), mailbox,
/// registers, reduction counter, links/monitors and exit reason. Signal delivery and GC
/// entry also live here; everything that mutates **two** processes at once lives on
/// [`crate::ptab::ProcessTable`] instead (see the crate `CONTEXT.md`, Design Decisions).
pub struct Process {
    /// This process's pid.
    pid: ProcessId,
    /// Scheduler-visible state.
    state: ProcessState,
    /// Boolean flags.
    flags: ProcessFlags,
    /// Scheduling priority (the legacy C `p->prio` / `PRIORITY_*`).
    priority: Priority,
    /// The legacy C `p->save_calls`: the number of most-recent calls kept for tracing. Only
    /// the value/return-old-value contract of `process_flag(save_calls, N)` is modelled (see
    /// [`Process::set_save_calls`]); the tracing subsystem itself is absent in M1.
    save_calls: u16,
    /// The two-generation collector owning the young (mutator) and mature heaps.
    ///
    /// The **strategy-dispatching** collector ([`beam_gc::AnyCollector`]): its mature generation
    /// is the copying space or the mark-region (GenImmix) space, chosen at construction by the
    /// automatic rule from the policy's initial mature size (or by an explicit pin when the
    /// caller builds it from a [`beam_gc::GcConfig`]). A default policy resolves to the copying
    /// space, so this is behaviour-identical to owning the concrete collector.
    collector: AnyCollector,
    /// Whether this process is **relocation-exempt** (default `false`).
    ///
    /// A host (e.g. `beam_rt`) may hold live terms from this process's young heap in Rust locals
    /// and nested-`Machine` registers that are **not** in the crate root set
    /// ([`Process::collect`] publishes only `x[0..live)`, the whole stack, the mailbox payloads,
    /// the process dictionary, the saved BIF/NIF args and the pending-signal queue). Relocating
    /// the heap against that incomplete root set would recycle live words — a wrong result. While
    /// exempt, [`Process::deliver`] surfaces the raw allocation refusal instead of collecting and
    /// growing, and [`Process::collect`]/[`Process::ensure_capacity`] refuse with a typed error
    /// (see [`Process::set_relocation_exempt`]).
    relocation_exempt: bool,
    /// The process's off-heap (MSO) handles.
    off_heap: OffHeapTable,
    /// The process's **owned** off-heap binary list: the reclamation home for the refc binaries
    /// whose ownership the process has taken ([`Process::register_off_heap_binary`]).
    ///
    /// This is a separate, **slot-stable** [`beam_gc::OffHeap`] from the one inside
    /// [`Process::off_heap`] (see the type-level docs on that table): the process's persistent
    /// `off_heap` table is the **message-transit / queued-signal** table the copy boundary writes
    /// into and *deliberately never sweeps*, whereas this list is the one list the process itself
    /// **sweeps** ([`Process::sweep_off_heap_binaries`], driven automatically at the end of a
    /// rooted collection). The two hold different objects (this one holds only [`RefcBinary`]
    /// payloads the process owns; `off_heap` holds MSO handles and the refc binaries that travel
    /// with a copied message/fragment), so no handle is ever owned twice — never register the same
    /// payload in both.
    owned_binaries: OffHeap,
    /// The index over the **live** entries of `owned_binaries`: a payload pointer
    /// ([`RefcBinary::data_ptr`] as `usize`, the frozen `BinRef.val` contract) → its slot.
    ///
    /// This is what makes [`Process::register_off_heap_binary`]'s dedup an **O(1)** lookup instead
    /// of a scan of the whole slot space: the map holds one entry per **live** payload, written on a
    /// fresh insert and pruned when a sweep releases an entry ([`Process::retain_off_heap_binaries`],
    /// the one release funnel), so it can never map a `data_ptr` to a released slot.
    owned_binary_index: HashMap<usize, usize>,
    /// The number of slots `owned_binaries` has ever handed out — i.e. the next fresh slot index.
    ///
    /// `beam_gc::OffHeap` exposes no "slot count" accessor, so this mirror (maintained only by
    /// [`Process::register_off_heap_binary`], the only operation that appends a slot) records the
    /// size of the slot space across sweeps: a sweep only marks slots released, so it never changes
    /// this count. Dedup resolves through [`Process::owned_binary_index`], not a scan over this
    /// count, so the mirror is now purely the slot space's size — the `next fresh slot` value a
    /// fresh insert is checked against (a `debug_assert!` in `register_off_heap_binary`).
    owned_binary_slots: usize,
    /// The payload pointers (`RefcBinary::data_ptr`) of the owned binaries the caller has
    /// **declared dead**: the reclamation worklist [`Process::sweep_off_heap_binaries`] consumes.
    /// It is a receiver-side, purely local list, never exposed to the heap.
    dead_binaries: Vec<usize>,
    /// The mailbox.
    mailbox: Queue,
    /// The root-set containers (x-registers + Erlang stack).
    regs: Registers,
    /// The per-process dictionary (`erlang:put/2`, `get/1`, `erase/1`, …): an in-heap
    /// sub-rootset the collector must scan (see [`Dict`]).
    dict: Dict,
    /// Saved BIF/NIF arguments: the last in-heap sub-rootset the collector scans (see
    /// [`SavedArgs`]).
    saved_args: SavedArgs,
    /// The decoupled pending-signal queue (see [`crate::signal_queue::SignalQueue`]): the exit
    /// signals and monitor-DOWN notifications generated *for* this process but not yet
    /// applied. A queued `reason` lives in this process's own heap, so it is an extra in-heap
    /// sub-rootset the collector must scan.
    signals: SignalQueue,
    /// Reductions consumed in the current time slice.
    reductions: u64,
    /// Outgoing links.
    links: Links,
    /// Outgoing monitors.
    monitors: Monitors,
    /// Outgoing **port** monitors: the ports this process watches
    /// (`erlang:monitor(port, Port)`).
    ///
    /// A [`PortWatch`] holds only plain values (a reference and a port number), **not** heap
    /// terms, so this set is **not** a GC root and adds nothing to
    /// [`Process::collect`]'s root set.
    port_monitors: Vec<PortWatch>,
    /// Incoming monitors (who monitors this process).
    monitored_by: Vec<MonitoredBy>,
    /// The termination reason, once the process has exited.
    exit_reason: Option<Eterm>,
}

/// A host-supplied predicate that can **decline a relocating collection** of a receiver's heap.
///
/// This is the host's half of the same exhaustion policy
/// [`Process::has_unpublished_roots`] implements in-crate. That crate-owned gate covers two cases:
/// a **relocation-exempt** process and a currently [`Running`](ProcessState::Running) one. A host
/// may know of a third — a receiver whose live root set is **not** fully scanned by its collector
/// (a root word, or a container the receiver's scanned graph reaches, living in a region the
/// collector does not scan) — and must be able to refuse the move. `beam_rt`'s unscanned-root
/// decline predicate (`beam_rt/src/context/gc.rs::root_is_unscanned` and
/// `src/context/gc_walk.rs::any_root_is_unscanned`, which it already consults at its own
/// heap-admission points) is the motivating one.
///
/// Answering `true` declines only the **relocation**: a no-relocation growth
/// ([`Process::grow_nursery`]) or a heap-fragment delivery is still permitted, so the notification
/// is **never lost** — it is delivered through the same heap-fragment route the `Running` /
/// relocation-exempt receiver already takes. A gate is installed on the process table
/// ([`ProcessTable::set_collection_gate`](crate::ptab::ProcessTable::set_collection_gate)) and is
/// consulted at every signal site in `crate::ptab`, plus by
/// [`ProcessTable::deliver`](crate::ptab::ProcessTable::deliver) — which honours it by marking the
/// receiver relocation-exempt for the duration of exactly one [`Process::deliver`] call, so the
/// copy engine's raw refusal reaches the caller unchanged.
///
/// The predicate takes `&mut self` so a gate can keep its own state (a cache, a counter), and
/// receives the receiver's PCB so it can consult the process itself (its pid, its heap, its
/// collector's geometry) rather than only an id.
pub trait CollectionGate {
    /// Whether a **relocating** collection of `receiver`'s heap must be declined.
    ///
    /// `true` refuses only the move: the caller routes the delivery to a heap fragment (or grows
    /// the nursery **without** relocating) and the message is still delivered.
    fn declines_relocation(&mut self, receiver: &Process) -> bool;
}

impl Process {
    /// A new process with pid `pid` and the default heap policy.
    ///
    /// # Errors
    ///
    /// Propagates heap-allocation failure when the initial spaces cannot be allocated.
    pub fn new(pid: ProcessId) -> Result<Process> {
        Process::with_policy(pid, HeapPolicy::default())
    }

    /// A new process with pid `pid` and an explicit heap-size `policy`.
    ///
    /// Spawns at the default [`Priority::Normal`] priority; use
    /// [`Process::with_policy_and_priority`] to be born at a non-normal priority.
    ///
    /// # Errors
    ///
    /// Propagates heap-allocation failure when the initial spaces cannot be allocated.
    pub fn with_policy(pid: ProcessId, policy: HeapPolicy) -> Result<Process> {
        Process::with_policy_and_priority(pid, policy, Priority::Normal)
    }

    /// A new process with pid `pid`, an explicit heap-size `policy` and an initial scheduling
    /// `priority`.
    ///
    /// This is how a runtime spawns a process that is **born** at a non-normal priority without
    /// a race: compose `table.alloc_pid()` + `Process::with_policy_and_priority(pid, policy,
    /// priority)` + `table.insert(process)` and then enqueue the pid at `priority`. The pid is
    /// not visible to any scheduler until `insert`, so the process is never requeued — nothing
    /// observes it at the wrong level. ([`crate::ptab::ProcessTable::spawn`] hard-sets
    /// [`Priority::Normal`]; a `spawn_opt(..., [{priority, P}])` therefore allocates the pid and
    /// inserts the PCB itself.)
    ///
    /// # Errors
    ///
    /// Propagates heap-allocation failure when the initial spaces cannot be allocated.
    pub fn with_policy_and_priority(
        pid: ProcessId,
        policy: HeapPolicy,
        priority: Priority,
    ) -> Result<Process> {
        // A freshly spawned process is runnable and schedulable: mark the `active` flag so
        // the rest of the runtime can tell a live PCB from a recycled slot.
        let mut flags = ProcessFlags::new();
        flags.set_active(true);
        Ok(Process {
            pid,
            state: ProcessState::Runnable,
            flags,
            priority,
            save_calls: 0,
            collector: AnyCollector::new(policy)?,
            relocation_exempt: false,
            off_heap: OffHeapTable::new(),
            owned_binaries: OffHeap::new(),
            owned_binary_index: HashMap::new(),
            owned_binary_slots: 0,
            dead_binaries: Vec::new(),
            mailbox: Queue::new(),
            regs: Registers::new(),
            dict: Dict::new(),
            saved_args: SavedArgs::new(),
            signals: SignalQueue::new(),
            reductions: 0,
            links: Links::new(),
            monitors: Monitors::new(),
            port_monitors: Vec::new(),
            monitored_by: Vec::new(),
            exit_reason: None,
        })
    }

    /// This process's pid.
    pub fn pid(&self) -> ProcessId {
        self.pid
    }

    /// The scheduler-visible state.
    pub fn state(&self) -> ProcessState {
        self.state
    }

    /// Sets the scheduler-visible state.
    pub fn set_state(&mut self, state: ProcessState) {
        self.state = state;
    }

    /// The atom `erlang:process_info(P, status)` reports for this process, or `None` when the
    /// process is gone (the BIF renders `undefined`).
    ///
    /// The mapping layers the transient `garbage_collecting` status over the scheduler state:
    ///
    /// * a **[`ProcessState::Terminated`]** process has no status at all -> `None`, matching the
    ///   oracle, whose entire `process_info` call returns `undefined` for a dead pid;
    /// * while the `garbage_collecting` flag is set the process is in GC, and ERTS reports
    ///   `garbage_collecting` **instead of** `running` — the flag is only meaningfully set while
    ///   the process is executing, so it is layered on top of the state;
    /// * otherwise the state's own atom ([`ProcessState::status_atom`]).
    ///
    /// Honest note on the oracle: the `garbage_collecting` window is transient and we could not
    /// force `erl` to expose it from a busy poll (only `runnable`/`running` were observed), so
    /// this mapping is defined from ERTS's documented status set plus this crate's
    /// `garbage_collecting` flag rather than from a captured oracle sample. The other five atoms
    /// are the oracle's own.
    pub fn status(&self) -> Option<&'static str> {
        if self.state == ProcessState::Terminated {
            return None;
        }
        if self.flags.garbage_collecting() {
            return Some("garbage_collecting");
        }
        self.state.status_atom()
    }

    /// Makes this process runnable if it is currently blocked.
    ///
    /// A process is "blocked" when its state is [`ProcessState::Waiting`] (a `receive` that
    /// found no matching message) or [`ProcessState::Suspended`] (an outside
    /// `erlang:suspend_process/1`). Both are woken by an arriving message: `Waiting` is the
    /// ordinary receive case and `Suspended` must still be seen to have a message so the
    /// suspension can be lifted later — this crate keeps them symmetric rather than guessing
    /// the historical C nuance.
    ///
    /// Returns `true` when the process was blocked and is now `Runnable`; `false` when it was
    /// already runnable/running/exiting and nothing changed.
    pub fn wake(&mut self) -> bool {
        match self.state {
            ProcessState::Waiting | ProcessState::Suspended => {
                self.state = ProcessState::Runnable;
                true
            }
            _ => false,
        }
    }
    /// The process's flags.
    pub fn flags(&self) -> &ProcessFlags {
        &self.flags
    }

    /// The process's flags, mutably.
    pub fn flags_mut(&mut self) -> &mut ProcessFlags {
        &mut self.flags
    }

    /// Whether the process traps exit signals (`trap_exit`).
    pub fn is_trapping_exit(&self) -> bool {
        self.flags.trap_exit()
    }

    /// Sets the `trap_exit` flag.
    pub fn set_trap_exit(&mut self, value: bool) {
        self.flags.set_trap_exit(value);
    }

    /// This process's scheduling priority (`process_flag(priority, P)`).
    pub fn priority(&self) -> Priority {
        self.priority
    }

    /// Sets this process's scheduling priority.
    ///
    /// This is a pure field set: moving an **already-enqueued** process between run-queue
    /// levels is the scheduler's job — the [`crate::scheduler::Scheduler`] reads
    /// [`Process::priority`] when it requeues on a yield, so a demotion made during a slice
    /// takes effect at the process's next requeue.
    pub fn set_priority(&mut self, priority: Priority) {
        self.priority = priority;
    }

    /// Whether this process is `sensitive` (`process_flag(sensitive, true)`).
    pub fn is_sensitive(&self) -> bool {
        self.flags.sensitive()
    }

    /// Sets the `sensitive` flag.
    pub fn set_sensitive(&mut self, value: bool) {
        self.flags.set_sensitive(value);
    }

    /// The value of the legacy C `p->save_calls` (`process_flag(save_calls, N)`).
    pub fn save_calls(&self) -> u16 {
        self.save_calls
    }

    /// Sets the `save_calls` count (`process_flag(save_calls, N)`).
    ///
    /// Only the *value* and the return-old-value contract of `process_flag(save_calls, N)`
    /// are modelled here so `beam_bifs` can reproduce the C-faithful returned old value. The
    /// *effect* — actually saving the process's last `N` calls for `seq_trace`/`dbg` — is NOT
    /// modelled in M1 because the tracing subsystem is absent.
    pub fn set_save_calls(&mut self, save_calls: u16) {
        self.save_calls = save_calls;
    }

    /// Reductions consumed in the current time slice.
    pub fn reductions(&self) -> u64 {
        self.reductions
    }

    /// Adds `n` to the reduction counter.
    pub fn bump_reductions(&mut self, n: u64) {
        self.reductions = self.reductions.wrapping_add(n);
    }

    /// Resets the reduction counter to zero.
    pub fn reset_reductions(&mut self) {
        self.reductions = 0;
    }

    /// The young (mutator) heap — where this process allocates terms.
    pub fn heap(&self) -> &Heap {
        self.collector.young().heap()
    }

    /// The young (mutator) heap, mutably.
    pub fn heap_mut(&mut self) -> &mut Heap {
        self.collector.young_mut().heap_mut()
    }

    /// The mailbox.
    pub fn mailbox(&self) -> &Queue {
        &self.mailbox
    }

    /// The mailbox, mutably.
    pub fn mailbox_mut(&mut self) -> &mut Queue {
        &mut self.mailbox
    }

    /// The root-set containers.
    pub fn registers(&self) -> &Registers {
        &self.regs
    }

    /// The root-set containers, mutably.
    pub fn registers_mut(&mut self) -> &mut Registers {
        &mut self.regs
    }

    /// The per-process dictionary (`erlang:put/2`/`get/1`/`erase/1`).
    pub fn dict(&self) -> &Dict {
        &self.dict
    }

    /// The per-process dictionary, mutably.
    pub fn dict_mut(&mut self) -> &mut Dict {
        &mut self.dict
    }

    /// The saved BIF/NIF arguments (an extra in-heap sub-rootset).
    pub fn saved_args(&self) -> &SavedArgs {
        &self.saved_args
    }

    /// The saved BIF/NIF arguments, mutably.
    pub fn saved_args_mut(&mut self) -> &mut SavedArgs {
        &mut self.saved_args
    }

    /// The decoupled pending-signal queue (see [`crate::signal_queue::SignalQueue`]).
    pub fn signals(&self) -> &SignalQueue {
        &self.signals
    }

    /// The pending-signal queue, mutably.
    pub fn signals_mut(&mut self) -> &mut SignalQueue {
        &mut self.signals
    }

    /// The number of signals queued for this process (pending-signal introspection).
    ///
    /// A non-zero count means the process has exit signals and/or monitor-DOWN notifications
    /// generated for it that have not been applied yet — i.e. they will be applied when this
    /// process is next scheduled in (or, for a process that is not currently executing, at the
    /// end of the sending operation).
    pub fn pending_signals(&self) -> usize {
        self.signals.len()
    }
    /// The process's off-heap (MSO) handles.
    pub fn off_heap(&self) -> &OffHeapTable {
        &self.off_heap
    }

    /// The process's off-heap (MSO) handles, mutably.
    pub fn off_heap_mut(&mut self) -> &mut OffHeapTable {
        &mut self.off_heap
    }

    /// Registers `binary` in the process's **owned** off-heap binary list and returns its slot.
    ///
    /// This is the process-path **binary-ownership home**: the process takes ownership of the
    /// payload (the list holds one reference to it) so that a later
    /// [`sweep_off_heap_binaries`](Process::sweep_off_heap_binaries) can reclaim it. The slot is
    /// **stable for the process's lifetime** — the list never shifts, reorders or reuses a slot —
    /// so a slot returned here stays resolvable
    /// ([`off_heap_binary`](Process::off_heap_binary)) across any number of sweeps.
    ///
    /// The list holds at most **one** reference per payload (the legacy VM's `MSO(p)` invariant): a
    /// payload already registered (same [`RefcBinary::data_ptr`], the frozen `BinRef.val`
    /// contract) keeps its existing slot and the surplus handle is dropped, so
    /// `register(handle.clone())` leaves the reference count unchanged. Because a payload therefore
    /// names exactly one slot, a declared-dead slot unambiguously names the payload to release.
    ///
    /// # The caller's contract
    ///
    /// The process cannot decide on its own whether an off-heap payload is still reachable — the
    /// on-heap `BinRef` that names it is nested inside boxed terms, and walking them is a
    /// collection's job, not this crate's. So registration only makes the process the *owner*; the
    /// payload is released only when the caller **declares it dead**
    /// ([`declare_off_heap_binary_dead`](Process::declare_off_heap_binary_dead)) — the one
    /// provable-death signal available without a heap traversal — or supplies a faithful predicate
    /// through [`retain_off_heap_binaries`](Process::retain_off_heap_binaries). Everything else is
    /// conservatively retained (leak-not-dangle). See `DESIGN_DECISIONS.md`.
    pub fn register_off_heap_binary(&mut self, binary: RefcBinary) -> usize {
        let data_ptr = binary.data_ptr() as usize;
        match self.owned_binary_slot(data_ptr) {
            // Already owned: the list keeps its single reference and the surplus handle drops.
            Some(slot) => slot,
            None => {
                let slot = self.owned_binaries.insert(OffHeapEntry::Binary(binary));
                // `beam_gc::OffHeap` is append-only and marks a released slot in place (it never
                // removes or reorders entries), so `entries.len() - 1` — the slot a fresh insert
                // hands out — is exactly the slot space's next index, i.e. the mirror's value
                // **before** this insert bumps it. Compare against that pre-insert mirror, not
                // against the value just written below.
                debug_assert_eq!(
                    slot, self.owned_binary_slots,
                    "a fresh insert hands out the slot space's next index"
                );
                self.owned_binary_index.insert(data_ptr, slot);
                self.owned_binary_slots = slot + 1;
                slot
            }
        }
    }

    /// The owned off-heap binary at `slot`, or `None` when the slot is out of range **or has been
    /// released** by a sweep (a released slot is never reused, so it can never resolve to an
    /// unrelated payload).
    pub fn off_heap_binary(&self, slot: usize) -> Option<&RefcBinary> {
        self.owned_binaries.get(slot).and_then(OffHeapEntry::as_binary)
    }

    /// The **live** owned off-heap binaries, in slot order (released slots skipped).
    pub fn off_heap_binaries(&self) -> impl Iterator<Item = &RefcBinary> {
        self.owned_binaries.iter().filter_map(OffHeapEntry::as_binary)
    }

    /// The number of **live** owned off-heap binaries (a released slot is not counted).
    pub fn off_heap_binary_count(&self) -> usize {
        self.owned_binaries.len()
    }

    /// The total payload bytes of the **live** owned off-heap binaries (the off-heap payload the
    /// process is charged for, the `system_info` memory accounting hook).
    pub fn off_heap_binary_bytes(&self) -> usize {
        self.owned_binaries.overhead_bytes()
    }

    /// Declares the owned off-heap binary at `slot` **dead** and returns whether a live entry was
    /// marked.
    ///
    /// This is the caller's **assertion** that nothing references the payload any more — no on-heap
    /// `BinRef`/`ErlSubBits` pair, and no other owner's handle. It is the only *provable-death*
    /// signal this crate can act on without a heap traversal, so it is deliberately explicit: the
    /// runtime (or the collector-side scan that owns liveness) is the party that can know it.
    ///
    /// The payload is **not** released here — the declaration is recorded and consumed by the next
    /// sweep ([`sweep_off_heap_binaries`](Process::sweep_off_heap_binaries)), which every rooted
    /// collection drives automatically. That deferred release is what makes the sweep the single
    /// place a death turns into a reclaim, so a run of declarations costs one sweep, not one sweep
    /// each; retaining a declared-dead payload until then is always safe (leak-not-dangle).
    ///
    /// `false` for an out-of-range or already-released slot (and for an entry that is not a binary:
    /// this list only ever registers binaries, so that case cannot arise from this API).
    pub fn declare_off_heap_binary_dead(&mut self, slot: usize) -> bool {
        let Some(binary) = self
            .owned_binaries
            .get(slot)
            .and_then(OffHeapEntry::as_binary)
        else {
            return false;
        };
        let data_ptr = binary.data_ptr() as usize;
        if !self.dead_binaries.contains(&data_ptr) {
            self.dead_binaries.push(data_ptr);
        }
        true
    }

    /// Reclaims the **declared-dead** owned off-heap binaries by driving
    /// [`beam_gc::OffHeap::sweep`] over the process's owned list, and returns how many entries were
    /// released.
    ///
    /// This is the process-path reclamation step. Every rooted collection runs it automatically
    /// (`run_rooted_collection`'s last step, after the roots are scattered back), and a host can
    /// also force it directly. Each released entry is dropped exactly once, so the payload is freed
    /// exactly at reference count 0 (and only when the process held the last handle); a released
    /// slot is never reused and a second sweep releases nothing further.
    ///
    /// # The predicate is CONSERVATIVE
    ///
    /// An entry is released **only** when it has been declared dead. Every other entry is retained,
    /// including an entry whose liveness the process cannot decide — the list is swept with a
    /// predicate (`release iff declared dead`) that can never report a possibly-live payload dead,
    /// because deciding liveness would need a heap traversal this crate does not have. The
    /// residual is a **leak, never a dangle**: a payload no caller declares stays owned by the
    /// process until the process dies (its `Drop` frees it) — see `DESIGN_DECISIONS.md` for the
    /// exact gap and its owner.
    pub fn sweep_off_heap_binaries(&mut self) -> usize {
        // Take the declarations: this sweep is the single place they turn into releases (the list
        // is cleared by the move, so the same payload is never declared twice).
        let dead = core::mem::take(&mut self.dead_binaries);
        self.retain_off_heap_binaries(|binary| -> bool {
            !dead.contains(&(binary.data_ptr() as usize))
        })
    }

    /// Reclaims every owned off-heap binary that **no reachable on-heap handle names** any more,
    /// and returns how many entries were released.
    ///
    /// This is the **reachability-driven** half of the off-heap reclamation seam. Instead of the
    /// conservative "release only what the caller declared dead"
    /// ([`Process::sweep_off_heap_binaries`]), it enumerates the process heap with
    /// [`beam_gc::AnyCollector::for_each_reachable_object_view`] and releases exactly the owned
    /// payloads whose [`RefcBinary::data_ptr`] — the frozen `BinRef.val` contract — is not named by
    /// any reachable `BinRef`. The walk hands each object over as a safe
    /// [`beam_gc::ReachableObject`] view, so this half needs no `unsafe` of its own. The release goes
    /// through the existing
    /// [`Process::retain_off_heap_binaries`] seam, so slot stability holds: a survivor keeps its
    /// original slot and a released slot is never reused.
    ///
    /// # The root set, and why nothing moves
    ///
    /// The walk runs over the **same** root set [`Process::collect`] gathers, because a handle the
    /// root set fails to publish would look unreachable: the registers/stack/extra roots
    /// ([`Registers::gather_roots`]), each queued message's `payload`/`from`/`token`, the process
    /// dictionary, the saved BIF/NIF arguments and the pending-signal queue. The mailbox is read
    /// **in place** ([`Queue::iter`]) rather than drained, so no message can be lost or reordered;
    /// the call is read-only and never collects, so it moves no term and needs no safepoint.
    ///
    /// # Why this REFUSES for a process with unpublished roots
    ///
    /// The verdict is only sound when the root set is **complete**. A **relocation-exempt**
    /// process ([`Process::set_relocation_exempt`]) or a currently **`Running`** one has live terms
    /// outside the crate root set (see [`Process::has_unpublished_roots`]), so the walk could miss
    /// a live `BinRef` and release a payload a real handle still names — a **dangle**. This method
    /// therefore retains everything and returns `0` for such a process; the conservative
    /// declared-dead path ([`Process::sweep_off_heap_binaries`]) stays available and is the
    /// fallback [`Process::run_rooted_collection`] uses for it.
    ///
    /// # Errors
    ///
    /// Propagates the walk's `BeamError` — it can only come from the shared reference-enumeration
    /// primitive, whose signature is fallible for its copier callers. Nothing is released when it
    /// errors, so the residual is a **leak, never a dangle**.
    pub fn sweep_off_heap_binaries_reachable(&mut self) -> Result<usize> {
        let mut roots: Vec<Eterm> = Vec::new();
        self.gather_scan_roots(&mut roots);
        self.sweep_off_heap_binaries_reachable_from(&roots)
    }

    /// The reachability-driven sweep over an **already gathered** root set — the shared body of
    /// [`Process::sweep_off_heap_binaries_reachable`] and the off-heap step of
    /// [`Process::run_rooted_collection`] (which reuses the roots its collection just gathered, so
    /// the walk can never disagree with the collection about what is reachable).
    ///
    /// Refuses (releasing nothing, `Ok(0)`) for a process whose root set is incomplete — see
    /// [`Process::sweep_off_heap_binaries_reachable`] for why.
    fn sweep_off_heap_binaries_reachable_from(&mut self, roots: &[Eterm]) -> Result<usize> {
        // Nothing owned: skip the walk entirely. The enumeration is O(heap), and a process that owns
        // no off-heap binary — the common case — must not pay for it on every collection.
        if self.owned_binaries.is_empty() {
            return Ok(0);
        }
        if self.has_unpublished_roots() {
            return Ok(0);
        }
        let mut reachable: HashSet<usize> = HashSet::new();
        self.collector
            .for_each_reachable_object_view(roots, |object| {
                if let Some(payload) = reachable_bin_ref_payload(&object) {
                    reachable.insert(payload);
                }
            })?;
        Ok(self.retain_off_heap_binaries(|binary| {
            reachable.contains(&(binary.data_ptr() as usize))
        }))
    }

    /// Gathers the process root set **read-only** — the scan-side gather for
    /// [`Process::sweep_off_heap_binaries_reachable`].
    ///
    /// The region list must stay in sync with the one [`Process::run_rooted_collection`] gathers
    /// (registers/stack/extra roots, then each message's `payload`/`from`/`token`, then the
    /// dictionary, the saved args and the pending signals); the two differ only in *how* they read
    /// the mailbox — this one peeks each message in place, the collection one drains it because it
    /// must write the relocated envelope words back.
    fn gather_scan_roots(&self, roots: &mut Vec<Eterm>) {
        self.regs.gather_roots(roots);
        for message in self.mailbox.iter() {
            roots.push(message.payload());
            roots.push(message.from());
            roots.push(message.token());
        }
        self.dict.gather_roots(roots);
        self.saved_args.gather_roots(roots);
        self.signals.gather_roots(roots);
    }
    /// Reclaims every **live** owned off-heap binary `is_live` reports dead, and returns how many
    /// entries were released.
    ///
    /// The caller-supplied-predicate half of the sweep seam, for a party that *can* decide liveness
    /// faithfully — a collector-side or host-side heap scan that walks the on-heap `BinRef`/MSO
    /// containers and reports which payload pointers it still found. The predicate is applied to
    /// each **live** entry (released slots are skipped), and the slot stability contract holds: a
    /// survivor keeps its original slot, a released slot answers `None` and is never reused.
    ///
    /// A caller that cannot walk the heap must not use this to guess: reporting a still-referenced
    /// payload dead releases the process's ownership of it, so a genuine `BinRef` would then dangle
    /// once the process's handle was the last one. Use
    /// [`declare_off_heap_binary_dead`](Process::declare_off_heap_binary_dead) for an asserted
    /// death and [`sweep_off_heap_binaries`](Process::sweep_off_heap_binaries) for the automatic
    /// conservative path.
    pub fn retain_off_heap_binaries<F>(&mut self, mut is_live: F) -> usize
    where
        F: FnMut(&RefcBinary) -> bool,
    {
        // Record the payload pointer of every entry this sweep releases, so the live index can be
        // pruned of exactly those — and only those — after the sweep returns.
        let mut released: Vec<usize> = Vec::new();
        let count = self.owned_binaries.sweep(|entry| match entry.as_binary() {
            Some(binary) => {
                let live = is_live(binary);
                if !live {
                    released.push(binary.data_ptr() as usize);
                }
                live
            }
            // This list only ever registers binaries; an MSO entry is foreign here — retain it.
            None => true,
        });
        // The index holds live entries only: drop the released payload pointers, so a released slot
        // can never resolve and a later re-registration of the same payload takes a fresh slot.
        for data_ptr in released {
            self.owned_binary_index.remove(&data_ptr);
        }
        count
    }

    /// The slot of the live owned binary whose payload starts at `data_ptr`, or `None`.
    ///
    /// This is an **O(1)** lookup in the live index [`Process::owned_binary_index`] — never a scan
    /// of the slot space, so registration cost is independent of how many slots the list has ever
    /// handed out. The [`RefcBinary::data_ptr`] identity (the frozen `BinRef.val` contract: the
    /// *first payload byte*) is what makes the list's slot↔payload association a bijection, so a
    /// declared-dead payload names exactly one slot. The index holds **live** entries only, pruned
    /// when a sweep releases one, so a released slot can never resolve.
    fn owned_binary_slot(&self, data_ptr: usize) -> Option<usize> {
        // One probe unit per resolution: the whole of this function's work.
        #[cfg(test)]
        probe_registration();
        self.owned_binary_index.get(&data_ptr).copied()
    }

    /// The process's young (mutator) heap **and** its off-heap (MSO) table, as two disjoint
    /// mutable borrows.
    ///
    /// [`Process::heap_mut`] and [`Process::off_heap_mut`] each take a whole `&mut self`, so a
    /// single caller cannot hold the two at once. This accessor exists precisely so such a caller
    /// can drive operations that need *both* simultaneously in one borrow — the motivating one is
    /// [`beam_message::Fragments::move_into`], which copies a fragmented message payload onto the
    /// destination heap while registering the copied payload's off-heap references in the
    /// destination table (`move_into(dst_heap, dst_off_heap)`), so a host can move a fragmented
    /// message onto a process's heap in one step.
    ///
    /// `self` is destructured internally, so the compiler proves the two references are disjoint
    /// borrows of distinct fields of `AnyCollector`/`Process`. Safe Rust only — no raw pointers and
    /// no `unsafe`.
    pub fn heap_and_off_heap_mut(&mut self) -> (&mut Heap, &mut OffHeapTable) {
        let Process {
            collector,
            off_heap,
            ..
        } = self;
        (collector.young_mut().heap_mut(), off_heap)
    }

    /// The process's young (mutator) heap **and** its mailbox, as two disjoint mutable borrows.
    ///
    /// [`Process::heap_mut`] and [`Process::mailbox_mut`] each take a whole `&mut self`, so a
    /// single caller cannot hold the two at once. This accessor exists precisely so such a caller
    /// can drive an operation that needs *both* simultaneously in one borrow — the motivating one
    /// is [`beam_message::send_fragmented`], which copies a payload onto the destination heap and
    /// enqueues the resulting message on the destination queue
    /// (`send_fragmented(from, term, to_heap, to_queue, ctx, fragment_capacity_words)`), so a host
    /// can drive the oversized-message fragment path straight into a process's own heap and mailbox
    /// without destructuring `Process` or reimplementing the enqueue.
    ///
    /// `self` is destructured internally, so the compiler proves the two references are disjoint
    /// borrows of distinct fields of `AnyCollector`/`Process`. Safe Rust only — no raw pointers and
    /// no `unsafe`.
    pub fn heap_and_mailbox_mut(&mut self) -> (&mut Heap, &mut Queue) {
        let Process {
            collector, mailbox, ..
        } = self;
        (collector.young_mut().heap_mut(), mailbox)
    }

    /// The outgoing links.
    pub fn links(&self) -> &Links {
        &self.links
    }

    /// The outgoing links, mutably.
    pub fn links_mut(&mut self) -> &mut Links {
        &mut self.links
    }

    /// The outgoing monitors.
    pub fn monitors(&self) -> &Monitors {
        &self.monitors
    }

    /// The outgoing monitors, mutably.
    pub fn monitors_mut(&mut self) -> &mut Monitors {
        &mut self.monitors
    }

    /// The monitors watching this process.
    pub fn monitored_by(&self) -> &[MonitoredBy] {
        &self.monitored_by
    }

    /// The outgoing **port** monitors: the ports this process watches
    /// (`erlang:monitor(port, Port)`).
    ///
    /// This is the outgoing half a port monitor needs for `erlang:demonitor/1` and for
    /// `erlang:process_info(P, monitors)`; the incoming half lives in the
    /// [`PortTable`](crate::port::PortTable). A [`PortWatch`] is a pair of plain values, so
    /// the set is not part of [`Process::collect`]'s root set.
    pub fn port_monitors(&self) -> &[PortWatch] {
        &self.port_monitors
    }

    /// The outgoing port monitors, mutably.
    pub fn port_monitors_mut(&mut self) -> &mut Vec<PortWatch> {
        &mut self.port_monitors
    }

    /// The monitors watching this process, mutably.
    pub fn monitored_by_mut(&mut self) -> &mut Vec<MonitoredBy> {
        &mut self.monitored_by
    }

    /// The termination reason, once the process has exited.
    pub fn exit_reason(&self) -> Option<Eterm> {
        self.exit_reason
    }

    /// Sets the termination reason.
    pub fn set_exit_reason(&mut self, reason: Eterm) {
        self.exit_reason = Some(reason);
    }

    /// Copies `term` into this process's young heap — resolving off-heap references
    /// against `src_off_heap` — and enqueues it as a message from `from`. If the process was
    /// blocked (`Waiting`/`Suspended`), it is woken and the returned [`DeliverOutcome`] is
    /// [`DeliverOutcome::Woke`].
    ///
    /// Retries once after an on-demand growth when the young heap (or the message table) is
    /// exhausted: `AllocKind::Heap`/`AllocKind::Message` exhaustion is a *normal*
    /// condition, not fatal OOM. The retry collects and grows the nursery to fit the refused
    /// request ([`Process::ensure_capacity`], seeded with the word count the copier reported),
    /// so a payload larger than the bounded nursery is still delivered — never duplicated and
    /// never truncated.
    ///
    /// **A receiver whose live terms may be outside the crate root set never takes that path**
    /// ([`Process::has_unpublished_roots`] — a **relocation-exempt** process, whose host holds live
    /// words outside the root set, or a **`Running`** one, whose own machine is executing and whose
    /// `regs` snapshot is therefore stale): the raw `AllocKind::Heap`/`AllocKind::Message` refusal
    /// is returned **unchanged** — no collection, no growth, no retry — because relocating would
    /// recycle live words. The raw refusal is exactly the signal the host's fragment fallback keys
    /// on, so the message is still delivered (through a fragment) without moving a word.
    ///
    /// # Errors
    ///
    /// Propagates the retry's own `BeamError::Alloc` — an `AllocKind::Message` refusal — when
    /// the payload still does not fit under the heap policy; for a receiver with unpublished roots
    /// it propagates the **first** `AllocKind::Heap`/`AllocKind::Message` refusal unchanged.
    pub fn deliver(
        &mut self,
        from: Eterm,
        term: Eterm,
        src_off_heap: &OffHeapTable,
    ) -> Result<DeliverOutcome> {
        match self.copy_message(from, term, src_off_heap) {
            Ok(()) => {}
            // A full young heap (or message table) is a normal condition: collect, grow the
            // nursery on demand and retry exactly once. Any other failure is propagated
            // untouched.
            Err(BeamError::Alloc(failure))
                if matches!(failure.kind, AllocKind::Heap | AllocKind::Message) =>
            {
                // A receiver with unpublished roots must NOT collect or grow: its heap cannot be
                // relocated against the crate root set — a relocation-exempt receiver's host holds
                // live words in Rust locals / nested-Machine registers, and a **running** receiver's
                // own machine holds them in x/stack/float registers that `regs` (a snapshot
                // refreshed only at a heap-admission point or a park) does not yet carry (see
                // [`Process::has_unpublished_roots`]). Surface the RAW refusal unchanged (`kind`
                // still Heap/Message) — that is exactly what the host's fragment fallback keys on
                // (`beam_rt::context::RuntimeContext::deliver_message` → `send_fragmented_message`),
                // so the message is delivered through a fragment with **no** heap word moving under
                // the unpublished handles.
                if self.has_unpublished_roots() {
                    return Err(BeamError::Alloc(failure));
                }
                // The copier reports the whole graph's flat word count in the failure (as
                // bytes) — for the preflight `AllocKind::Message` refusal and the mid-copy
                // `AllocKind::Heap` one alike — so the words the growth must cover are
                // recoverable from it.
                let need_words = failure.requested / core::mem::size_of::<Eterm>();
                // Collect once and grow the nursery explicitly to fit `need_words`: a
                // collection sizes the nursery from what it left alive in it, not from what
                // this request is about to allocate, so an oversized payload needs the
                // on-demand growth (`Process::ensure_capacity`).
                //
                // This branch is reached only for a receiver whose live terms ARE in the crate
                // root set (not exempt, not `Running`): any other returned above (no collect, no
                // growth, no retry), so the heap is never moved under unpublished roots.                //
                // The growth's own error is deliberately ignored. When the policy's ceiling
                // makes the request unreachable, `ensure_capacity` fails **fail-fast** with
                // `AllocKind::Heap` without touching the collector; the retry below then
                // reproduces the refusal as the raw `AllocKind::Message` the caller keys its
                // fragment fallback on (`beam_rt::deliver_message`), so surfacing the growth's
                // `AllocKind::Heap` error here would change the caller-visible contract. This
                // is not a silent swallow — the retry's own result is authoritative and is
                // what the delivery returns.
                let _growth = self.ensure_capacity(need_words);
                self.copy_message(from, term, src_off_heap)?;
            }
            Err(error) => return Err(error),
        }
        Ok(if self.wake() {
            DeliverOutcome::Woke
        } else {
            DeliverOutcome::Enqueued
        })
    }

    /// The no-off-heap convenience form of [`Process::deliver`].
    ///
    /// # Errors
    ///
    /// Propagates the underlying `BeamError::Alloc` if the copy still does not fit after a
    /// collection.
    pub fn deliver_simple(&mut self, from: Eterm, term: Eterm) -> Result<DeliverOutcome> {
        // An empty source table: nothing to resolve, so the copy shares immediates and
        // copies the rest verbatim.
        let no_off_heap = OffHeapTable::new();
        self.deliver(from, term, &no_off_heap)
    }
    /// Copies `term` — whose off-heap references are described by `src_off_heap` — into this
    /// process's own young heap, registering every referenced off-heap object in *this*
    /// process's off-heap table so it outlives the copy, and returns the copied term.
    ///
    /// Unlike [`Process::deliver`] this enqueues nothing: the caller adopts the copied term
    /// itself (the exit-reason cascade does, so a propagated reason never points into a heap
    /// that is about to be dropped). Destructures `self` so the young heap and the off-heap
    /// table are borrowed disjointly.
    ///
    /// # Errors
    ///
    /// Propagates the copy engine's `BeamError::Alloc` when the term does not fit.
    pub(crate) fn copy_into_heap(&mut self, term: Eterm, src_off_heap: &OffHeapTable) -> Result<Eterm> {
        let Process {
            collector,
            off_heap,
            ..
        } = self;
        let to_heap = collector.young_mut().heap_mut();
        let mut ctx = CopyContext::new(src_off_heap, off_heap);
        copy_struct(term, to_heap, &mut ctx)
    }

    /// Removes and returns the first queued message whose payload satisfies `pred`.
    ///
    /// Non-matching messages stay queued in FIFO order (selective receive). Returns `None`
    /// when nothing matches — actually *waiting* is the scheduler's job.
    pub fn receive<F: FnMut(&Message) -> bool>(&mut self, pred: F) -> Option<Message> {
        beam_message::receive(&mut self.mailbox, pred)
    }

    /// The process's garbage collector.
    ///
    /// The strategy-dispatching [`beam_gc::AnyCollector`]: its mature generation is whichever
    /// strategy the policy's configuration resolved to ([`beam_gc::AnyCollector::kind`] names
    /// it), so a caller inspects the mature space through the geometry accessors
    /// ([`beam_gc::AnyCollector::mature_capacity_words`] and its siblings) rather than a
    /// strategy-specific `mature()` reference.
    pub fn collector(&self) -> &AnyCollector {
        &self.collector
    }

    /// Whether the young heap can no longer satisfy `need_words` more words.
    pub fn should_collect(&self, need_words: usize) -> bool {
        self.collector.should_collect(need_words)
    }

    /// The number of words used in the young heap.
    pub fn heap_used_words(&self) -> usize {
        self.collector.young().used_words()
    }

    /// Marks this process as **relocation-exempt** (or clears the mark).
    ///
    /// A host (the `beam_rt` runtime is the motivating one) can hold live terms from this
    /// process's young heap in Rust locals and nested-`Machine` registers that are **not** in the
    /// crate root set: [`Process::collect`] publishes only the x-registers/stack, the mailbox
    /// payloads, the process dictionary, the saved BIF/NIF args and the pending-signal queue, so a
    /// term held by the `-eval` evaluator's environment or by a nested `Machine` would not be
    /// relocated and its words would be recycled — a **wrong result**. While exempt:
    ///
    /// * [`Process::deliver`] does **not** collect or grow on an allocation refusal: it returns the
    ///   raw `BeamError::Alloc(AllocKind::Heap | AllocKind::Message)` unchanged, which is exactly
    ///   what the host's fragment fallback keys on (so the oversized message is delivered through a
    ///   fragment and no heap word moves under the host's unpublished handles);
    /// * [`Process::collect`] and [`Process::ensure_capacity`] refuse with the typed
    ///   `relocation_exempt` raise rather than relocating against an incomplete root set;
    /// * [`Process::collect_if_needed`] is a no-op answering `Ok(false)` (its return value means
    ///   "a collection ran").
    ///
    /// The exemption is **per process** and defaults to `false`, so a process the host does not
    /// explicitly mark behaves exactly as before. Enabling it is the host's assertion that *every*
    /// live term from this process is reachable through the crate root set (or is otherwise safe to
    /// leave un-relocated).
    ///
    /// This flag is **one of the two** cases of the gate every delivery-driven path tests —
    /// [`Process::has_unpublished_roots`], whose other case is a [`ProcessState::Running`] process
    /// (whose own machine's live terms are unpublished mid-run).
    pub fn set_relocation_exempt(&mut self, exempt: bool) {
        self.relocation_exempt = exempt;
    }

    /// Whether this process is **relocation-exempt** (default `false`).
    ///
    /// See [`Process::set_relocation_exempt`] for the contract the flag expresses.
    pub fn is_relocation_exempt(&self) -> bool {
        self.relocation_exempt
    }

    /// Whether this process's live terms may be **outside** the crate's root set, so a
    /// **delivery-driven** collect/grow ([`Process::deliver`] and the signal builders in
    /// `ptab.rs`) must be refused for it and the delivery routed to the heap-fragment model.
    ///
    /// The gate is true in exactly two cases:
    ///
    /// * the process is **relocation-exempt** ([`Process::set_relocation_exempt`]) — the host holds
    ///   live terms from this young heap in Rust locals / nested-`Machine` registers that the root
    ///   set does not publish;
    /// * the process state is [`ProcessState::Running`] — the single-threaded scheduler sets that
    ///   state for the one process whose own engine machine is executing
    ///   (`crate::scheduler::slice_begin`/`slice_end`), and a running machine's live terms are
    ///   **not** in the root set: [`Process::regs`] is a *host-published snapshot*, refreshed only
    ///   at the engine's heap-admission points and at a park, never between them. A delivery-driven
    ///   collection would therefore relocate the heap against a **stale** snapshot and recycle
    ///   words the running machine (and a `beam_bifs` fast-path BIF's Rust-local accumulator) still
    ///   points at.
    ///
    /// Refusing the collect/grow is **safe and bounded**: the delivery either fits the current
    /// nursery (no relocation, no corruption) or takes the heap-fragment fallback, and the running
    /// process's own next heap-admission point publishes the **current** roots and collects — so
    /// nothing accumulates unboundedly. Only the **delivery-driven** entry points are gated:
    /// [`Process::collect`]/[`Process::ensure_capacity`] are the host's admission points, which
    /// publish the roots *before* calling them.
    pub fn has_unpublished_roots(&self) -> bool {
        self.relocation_exempt || self.state == ProcessState::Running
    }

    /// Runs a safepoint collection over the whole process root set.
    ///
    /// The root set is the registers/stack **plus** the host-published extra roots
    /// ([`Registers::gather_roots`], which appends `x[0..live)`, then the stack, then
    /// [`Registers::extra_roots`] — the terms a host holds outside `x`/`stack`, e.g. a parked
    /// engine machine's live pending-exception reason term) **plus** the
    /// mailbox message-envelope words, the process dictionary ([`Dict::gather_roots`]), the
    /// saved BIF/NIF arguments ([`SavedArgs::gather_roots`]) and the pending-signal queue
    /// ([`SignalQueue::gather_roots`]) — all in-heap sub-rootsets: the queue is drained, each
    /// message's `payload`/`from`/`token` and the dict/saved-arg/signal words are appended to
    /// the root buffer, the collector runs, and each region is written back (the registers slice
    /// via [`Registers::scatter_roots`], each message re-installed through [`Message::with_payload`]
    /// with its relocated `from`/`token` written back through
    /// [`Message::set_from`]/[`Message::set_token`] — its off-heap handles/refc binaries/node
    /// entries/fragment chain travel with the [`Message::with_payload`] rebuild — then the dict,
    /// the saved args and the pending signals scattered back) in the SAME order it was gathered.
    ///
    /// A **fragment-resident** payload (a message too large for the receiver's heap) points
    /// into the message's own [`Fragments`], not into the nursery being collected, so the
    /// collector classifies it as foreign to every evacuated range and leaves it exactly where
    /// it is; the message keeps owning the fragment chain that holds it.
    ///
    /// The last step is the process's off-heap refc-binary reclamation, run after every root region
    /// is back in place. It has two halves: the **reachability** sweep
    /// ([`Process::sweep_off_heap_binaries_reachable_from`]) releases every owned payload no
    /// reachable on-heap `BinRef` names any more when the root set is complete, and the
    /// **conservative** declared-dead sweep ([`Process::sweep_off_heap_binaries`]) always runs — so
    /// a payload the caller has declared dead is released, and one the walk could not soundly judge
    /// (a process with unpublished roots) is retained.    ///
    /// The outgoing port-monitor set ([`Process::port_monitors`]) is deliberately **not**
    /// gathered: a [`PortWatch`] holds a reference and a port number, both plain values
    /// rather than heap terms, so it cannot be relocated and needs no root.
    ///
    /// A **relocation-exempt** process ([`Process::set_relocation_exempt`]) must **not** be
    /// collected: the crate root set is incomplete for it (the host holds live words outside it),
    /// so relocating would recycle live terms. Instead of a fabricated
    /// [`Collection`](beam_gc::Collection) this returns the typed `relocation_exempt` raise, and
    /// [`Process::run_rooted_collection`] is never entered.
    ///
    /// # Errors
    ///
    /// Propagates the collector's `BeamError`s; the typed `relocation_exempt` raise for an
    /// exempt process.
    pub fn collect(&mut self) -> Result<Collection> {
        if self.relocation_exempt {
            return Err(error::relocation_exempt("collect"));
        }
        self.run_rooted_collection(|collector, roots| collector.collect(roots))
    }

    /// Grows the young heap **on demand** so that a `need_words`-word allocation can proceed —
    /// the sibling of [`Process::collect`] and the runtime's explicit growth seam (the one
    /// [`Process::deliver`] drives on its refusal path).
    ///
    /// It runs one collection over the same root set [`Process::collect`] gathers (a minor,
    /// escalating to a major when a minor cannot make room), then grows the now-empty nursery
    /// to the smallest curated size covering `need_words`; afterwards
    /// [`young().available_words()`](beam_gc::HeapSpace::available_words) `>= need_words`. A
    /// collection sizes the nursery from what it left alive in it, not from what the caller is
    /// about to allocate, so this explicit step is what lets a request larger than the
    /// (otherwise **bounded**) nursery be met — the capacity never grows merely because a
    /// collection ran.
    ///
    /// # Errors
    ///
    /// When [`max_heap_words`](beam_gc::HeapPolicy::max_heap_words) (or the growth table's
    /// ceiling) makes the request unreachable, the typed [`BeamError::Alloc`] is returned
    /// **fail-fast**, before anything is collected, leaving the collector untouched; otherwise
    /// the collector's `BeamError`s are propagated.
    ///
    /// A **relocation-exempt** process ([`Process::set_relocation_exempt`]) must **not** be grown
    /// (growing runs a collection first), so this returns the typed `relocation_exempt` raise
    /// without touching the collector — see [`Process::collect`].
    pub fn ensure_capacity(&mut self, need_words: usize) -> Result<Collection> {
        if self.relocation_exempt {
            return Err(error::relocation_exempt("ensure_capacity"));
        }
        self.run_rooted_collection(|collector, roots| collector.ensure_capacity(need_words, roots))
    }

    /// A **no-relocation** growth passthrough: enlarges the young nursery so a `need_words`-word
    /// allocation can proceed, **without collecting and without moving any live term** —
    /// [`beam_gc::AnyCollector::grow_nursery`] forwarded for the process's own collector
    /// (strategy-agnostic across the mature-generation dispatch).
    ///
    /// The space the growth replaces is **retained** (the nursery becomes a chain of spaces) and
    /// never collapses here, so every live word stays addressable at its **original address** and
    /// no collection runs.
    ///
    /// Because nothing moves, this call is **not** gated by
    /// [`relocation_exempt`](Process::set_relocation_exempt) and carries **no** relocation-exempt
    /// guard: unlike [`Process::collect`] / [`Process::ensure_capacity`] (which collect first and
    /// so refuse an exempt process with the typed `relocation_exempt` raise), this call is legal
    /// and succeeds for an exempt process — which is exactly the point, because a host holding an
    /// incomplete or unpublished root set (the `-eval` evaluator, a nested `Machine`) may call it
    /// safely.
    ///
    /// Returns the resulting young-nursery capacity in words
    /// ([`beam_gc::HeapSpace::capacity_words`] of the now-current nursery space).
    ///
    /// # Errors
    ///
    /// When [`max_heap_words`](beam_gc::HeapPolicy::max_heap_words) (or the growth table's
    /// ceiling) cannot reach `need_words`, the typed [`BeamError::Alloc`] heap-exhaustion error is
    /// returned and the collector is left untouched; it is propagated unchanged (never swallowed).
    pub fn grow_nursery(&mut self, need_words: usize) -> Result<usize> {
        self.collector.grow_nursery(need_words)?;
        Ok(self.collector.young().capacity_words())
    }

    /// Gathers the process root set, runs `collect` over it and writes every region back — the
    /// shared body of [`Process::collect`] and [`Process::ensure_capacity`].
    ///
    /// `collect` is the collection action: [`beam_gc::AnyCollector::collect`] for a plain
    /// collection, or [`beam_gc::AnyCollector::ensure_capacity`] for an on-demand nursery growth.
    /// Every region is written
    /// back **even when the action returned `Err`**: an `ensure_capacity` failure can happen
    /// *after* its collection succeeded (the nursery growth was refused), at which point the
    /// roots have already been relocated in place, so leaving them unwritten would corrupt the
    /// heap. On a fail-fast / pre-copy failure the roots are un-relocated and the write-back is
    /// a harmless no-op (it rewrites the same values and re-installs the same payloads).
    ///
    /// Each queued message contributes **three** envelope words to the root set — its `payload`,
    /// its `from` and its `token`, in that order — and all three are written back afterwards. The
    /// common case is an immediate sender pid / node atom / sysname, but the envelope is a general
    /// term seam: a host may store a heap word in either slot (the C allows a heap `token` tuple
    /// `erl_message.c:388-390` and a heap `Info`-tuple `from` `erl_proc_sig_queue.c:2272`), and an
    /// un-relocated heap word would dangle into the evacuated from-space.
    /// [`Message::with_payload`] rebuilds the message (the relocation-safe form, which carries the
    /// off-heap handle list, the refc-binary list, the node list and the [`Fragments`] chain
    /// across), and [`Message::set_from`]/[`Message::set_token`] write the relocated envelope
    /// words back into it before the rebuilt message is enqueued. Rebuilding from scratch would
    /// drop the fragment chain — a use-after-free, since the payload would then point into freed
    /// fragments — and would leak the refcounts the message held on its off-heap objects.
    fn run_rooted_collection<F>(&mut self, collect: F) -> Result<Collection>
    where
        F: FnOnce(&mut AnyCollector, &mut [Eterm]) -> Result<Collection>,
    {
        // 1. The flat register slice: `x[0..live)`, then the whole stack, then the
        //    host-published extra roots (`regs.extra_roots()` — a parked engine machine's live
        //    terms, e.g. its pending-exception reason), in that fixed order. `reg_words` is both
        //    where the mailbox roots start and the length of the register slice `scatter_roots`
        //    is handed back below.
        let mut roots: Vec<Eterm> = Vec::new();
        let x_live = self.regs.gather_roots(&mut roots);
        let reg_words = roots.len();

        // 2. Drain the mailbox, appending each message's `payload`, `from` and `token` as roots
        //    (MESSAGE_ROOT_WORDS words per message, in that order). All three are relocatable: a
        //    host may store a heap word in `from`/`token` (the C allows a heap `token` tuple and a
        //    heap `Info`-tuple `from`), and the payload is the message body. The drained messages
        //    are re-installed around their relocated words below.
        let mut drained: Vec<Message> = Vec::new();
        while let Some(message) = self.mailbox.dequeue() {
            roots.push(message.payload());
            roots.push(message.from());
            roots.push(message.token());
            drained.push(message);
        }
        let mail_words = roots.len();

        // 3. The process dictionary, the saved BIF/NIF arguments and the pending-signal queue
        //    — the remaining in-heap sub-rootsets. A queued signal's `reason` lives in this
        //    heap (it is copied in at enqueue time), so it must be relocated with the rest.
        //    `dict_start`/`saved_start`/`signals_start` are the offsets their scatter steps
        //    take.
        let dict_start = self.dict.gather_roots(&mut roots);
        let saved_start = self.saved_args.gather_roots(&mut roots);
        let signals_start = self.signals.gather_roots(&mut roots);

        // 4. Run the collection action. Its result is returned *after* the write-back below,
        //    because the roots may already have been relocated in place even when it is `Err`.
        let action = collect(&mut self.collector, &mut roots);

        // 5. Write the (relocated) register, stack and extra-root words back. The slice handed
        //    in is exactly the register region `gather_roots` appended (`x` + stack + extra
        //    roots) — the mailbox/dict/saved-arg/signal regions that follow are written back by
        //    their own scatter steps, so the extra-root tail must not be confused with them.
        self.regs.scatter_roots(&roots[..reg_words], x_live);

        // 6. Re-install every message around its relocated envelope words, preserving FIFO order.
        //    A message's three roots are `[payload, from, token]` at `index * MESSAGE_ROOT_WORDS`,
        //    so `set_from`/`set_token` write the relocated envelope words into the owned message
        //    first and `with_payload` (which consumes it) swaps in the relocated payload last.
        //    The off-heap handles, refc binaries, node entries and — for an oversized payload —
        //    its fragment chain all travel across the rebuild (a fragment-resident payload is
        //    outside the collected nursery, so the collector classified it `Foreign` and left it
        //    untouched).
        let words = &roots[reg_words..mail_words];
        for (index, mut message) in drained.into_iter().enumerate() {
            let base = index * MESSAGE_ROOT_WORDS;
            message.set_from(words[base + MESSAGE_ROOT_FROM]);
            message.set_token(words[base + MESSAGE_ROOT_TOKEN]);
            self.mailbox
                .enqueue(message.with_payload(words[base + MESSAGE_ROOT_PAYLOAD]));
        }

        // 7. Write the relocated dictionary, saved-argument and pending-signal words back, in
        //    gather order.
        self.dict.scatter_roots(&roots, dict_start);
        self.saved_args.scatter_roots(&roots, saved_start);
        self.signals.scatter_roots(&roots, signals_start);

        // 8. Reclaim the off-heap binaries, over the roots gathered above. Two halves:
        //
        //    * the **reachability** sweep (`sweep_off_heap_binaries_reachable_from`) releases every
        //      owned payload no reachable on-heap `BinRef` names any more — sound only when the
        //      root set is complete, so it refuses (signalling `Ok(0)` and releasing nothing) for a
        //      process with unpublished roots (`has_unpublished_roots`), where the walk could miss
        //      a live handle and release its payload — a dangle;
        //    * the **conservative declared-dead** sweep (`sweep_off_heap_binaries`) always runs, so
        //      a caller's asserted death is honoured whether or not the reachability half was sound
        //      (its declarations are unreachable by definition, and an entry the reachability half
        //      released is simply skipped as an already-released slot).
        //
        //    Both are deliberately *after* the write-back: the roots are consistent again, and
        //    releasing a payload never moves a heap word. A no-op for a process that owns no
        //    off-heap binaries.
        let reclamation = self.sweep_off_heap_binaries_reachable_from(&roots);
        self.sweep_off_heap_binaries();

        // The collection's own result is what the caller asked for, and it is reported even when
        // it is `Err`; a reclamation failure only surfaces when the collection itself succeeded
        // (the roots are already back in place either way, so the heap stays consistent).
        match action {
            Ok(collection) => {
                reclamation?;
                Ok(collection)
            }
            Err(err) => Err(err),
        }
    }
    /// Collects first when `need` more words do not fit, then reports whether a collection
    /// ran.
    ///
    /// A **relocation-exempt** process ([`Process::set_relocation_exempt`]) never collects here
    /// (its heap must not be relocated against an incomplete root set), so this returns
    /// `Ok(false)` — "no collection ran" — regardless of `need`. The caller must already handle
    /// the not-enough-room case (the host's fragment fallback).
    ///
    /// # Errors
    ///
    /// Propagates the collector's errors.
    pub fn collect_if_needed(&mut self, need: usize) -> Result<bool> {
        if self.relocation_exempt {
            return Ok(false);
        }
        if self.should_collect(need) {
            self.collect()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// The copy half of [`Process::deliver`]: copies `term` into the young heap through the
    /// shared copy engine and enqueues the resulting message.
    ///
    /// Destructures `self` so the young heap and the mailbox can be borrowed disjointly (a
    /// `&mut self` method cannot hold both borrows).
    ///
    /// # The staging table is per-send, not the process's own
    ///
    /// `beam_message::send` **drains** the copy's destination off-heap table into the [`Message`]
    /// it builds (`enqueue_copied`), because that destination *is* the message's off-heap
    /// ownership. It must therefore be a **fresh, empty staging** table — never `self.off_heap`.
    /// `Process.off_heap` is instead the process's **persistent** off-heap table: a queued
    /// signal's reason is registered there by [`Process::copy_into_heap`] and stays for the
    /// process's lifetime (its on-heap `PendingSignal.reason` references those slots), and
    /// [`Process::heap_and_off_heap_mut`] lets a host register moved-fragment objects in it too.
    /// Passing it here would *steal* those live handles into this message on every send, leaving
    /// the queued signal referencing freed slots/payloads — a use-after-free. `beam_rt`'s
    /// `send_fragmented_message` stages the same way (`let mut dst = OffHeapTable::new();`).
    ///
    /// A failed copy wrote nothing to the staging table (the copier preflights capacity before
    /// writing a word), so the two-attempt retry in [`Process::deliver`] stays correct: each
    /// attempt builds its own fresh table.
    fn copy_message(&mut self, from: Eterm, term: Eterm, src_off_heap: &OffHeapTable) -> Result<()> {
        let Process {
            collector,
            mailbox,
            ..
        } = self;
        let to_heap = collector.young_mut().heap_mut();
        // A fresh per-send staging table: `send` drains it into the message, so it must start
        // empty and must NOT be the process's persistent `off_heap` (see above).
        let mut dst = OffHeapTable::new();
        let mut ctx = CopyContext::new(src_off_heap, &mut dst);
        beam_message::send(from, term, to_heap, mailbox, &mut ctx)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the PCB: `ProcessFlags` bits, the state/flag/reduction accessors,
    //! the link/monitor accessors and the collect policy. Message delivery and the full
    //! safepoint collection are exercised by the `tests/` integration suite.

    use beam_term::prelude::{make_small, ptr_val, tuple_element, tuple_size, AtomTable, NIL};

    use super::*;
    use crate::monitor::{Monitor, MonitorKind, RefAllocator};
    use crate::pid::PortId;
    use crate::signal::SignalAtoms;

    fn pid(number: u64) -> ProcessId {
        ProcessId::new(number, 0)
    }

    #[test]
    fn flags_default_to_none_and_round_trip_through_bits() {
        let mut flags = ProcessFlags::new();
        assert_eq!(flags.bits(), 0);
        assert!(!flags.trap_exit());
        assert!(!flags.system());
        assert!(!flags.in_run_queue());
        assert!(!flags.garbage_collecting());
        assert!(!flags.active());

        flags.set_trap_exit(true);
        flags.set_system(true);
        flags.set_in_run_queue(true);
        flags.set_garbage_collecting(true);
        flags.set_active(true);
        assert!(flags.trap_exit() && flags.system() && flags.in_run_queue());
        assert!(flags.garbage_collecting() && flags.active());
        assert_eq!(flags.bits(), 0b1_1111);

        // Clearing one bit leaves the others alone (`system` is bit 1).
        flags.set_system(false);
        assert!(!flags.system());
        assert!(flags.trap_exit());
        assert_eq!(flags.bits(), 0b1_1101);

        let rebuilt = ProcessFlags::from_bits(0b1_1101);
        assert_eq!(rebuilt.bits(), flags.bits());
        assert!(rebuilt.trap_exit());
    }

    #[test]
    fn sensitive_flag_round_trips() {
        let mut flags = ProcessFlags::new();
        assert!(!flags.sensitive());
        flags.set_sensitive(true);
        assert!(flags.sensitive());
        assert_eq!(flags.bits(), FLAG_SENSITIVE);
        flags.set_sensitive(false);
        assert!(!flags.sensitive());
        assert_eq!(flags.bits(), 0);
    }

    #[test]
    fn default_priority_is_normal_and_round_trips() {
        let mut process = Process::new(pid(11)).expect("the default heap allocates");
        assert_eq!(process.priority(), Priority::Normal);
        process.set_priority(Priority::Max);
        assert_eq!(process.priority(), Priority::Max);
        process.set_priority(Priority::Low);
        assert_eq!(process.priority(), Priority::Low);
    }

    #[test]
    fn every_process_state_maps_to_its_oracle_status_atom() {
        assert_eq!(ProcessState::Running.status_atom(), Some("running"));
        assert_eq!(ProcessState::Runnable.status_atom(), Some("runnable"));
        assert_eq!(ProcessState::Waiting.status_atom(), Some("waiting"));
        assert_eq!(ProcessState::Suspended.status_atom(), Some("suspended"));
        assert_eq!(ProcessState::Exiting.status_atom(), Some("exiting"));
        assert_eq!(
            ProcessState::Terminated.status_atom(),
            None,
            "a dead process has no status"
        );
    }

    #[test]
    fn status_layers_garbage_collecting_over_the_state() {
        let mut process = Process::new(pid(20)).expect("the default heap allocates");

        // A fresh process is runnable.
        assert_eq!(process.status(), Some("runnable"));

        // The `garbage_collecting` flag overrides the state while it is set (the flag is only
        // meaningfully set while the process is executing).
        process.set_state(ProcessState::Running);
        assert_eq!(process.status(), Some("running"));
        process.flags_mut().set_garbage_collecting(true);
        assert_eq!(process.status(), Some("garbage_collecting"));
        process.flags_mut().set_garbage_collecting(false);
        assert_eq!(process.status(), Some("running"));

        // Every non-terminated state still reports its own atom with the flag cleared.
        for state in [
            ProcessState::Runnable,
            ProcessState::Waiting,
            ProcessState::Suspended,
            ProcessState::Exiting,
        ] {
            process.set_state(state);
            assert_eq!(process.status(), state.status_atom());
        }

        // A terminated process reports `None` — even with the flag (stale) set.
        process.set_state(ProcessState::Terminated);
        assert_eq!(process.status(), None);
        process.flags_mut().set_garbage_collecting(true);
        assert_eq!(
            process.status(),
            None,
            "a terminated process has no status at all"
        );
    }

    #[test]
    fn with_policy_and_priority_births_at_the_given_priority() {
        // The default constructor and `with_policy` both spawn at normal ...
        let default = Process::new(pid(21)).expect("the default heap allocates");
        assert_eq!(default.priority(), Priority::Normal);
        let policy = Process::with_policy(pid(22), HeapPolicy::default())
            .expect("the default heap allocates");
        assert_eq!(policy.priority(), Priority::Normal);

        // ... while `with_policy_and_priority` births the process at the requested level.
        let high = Process::with_policy_and_priority(pid(23), HeapPolicy::default(), Priority::High)
            .expect("the default heap allocates");
        assert_eq!(high.priority(), Priority::High);
        assert_eq!(high.state(), ProcessState::Runnable);
        assert!(high.flags().active(), "a spawned process is active");
        assert_eq!(high.pid(), pid(23));
    }

    #[test]
    fn sensitive_accessors_delegate_to_the_flags() {
        let mut process = Process::new(pid(12)).expect("the default heap allocates");
        assert!(!process.is_sensitive());
        process.set_sensitive(true);
        assert!(process.is_sensitive());
        assert!(process.flags().sensitive());
        process.set_sensitive(false);
        assert!(!process.is_sensitive());
    }

    #[test]
    fn save_calls_defaults_to_zero_and_round_trips() {
        let mut process = Process::new(pid(13)).expect("the default heap allocates");
        assert_eq!(process.save_calls(), 0);
        process.set_save_calls(100);
        assert_eq!(process.save_calls(), 100);
        process.set_save_calls(0);
        assert_eq!(process.save_calls(), 0);
    }

    #[test]
    fn a_new_process_is_runnable_and_active() {
        let process = Process::new(pid(1)).expect("the default heap allocates");
        assert_eq!(process.pid(), pid(1));
        assert_eq!(process.state(), ProcessState::Runnable);
        assert!(process.flags().active());
        assert!(!process.is_trapping_exit());
        assert_eq!(process.reductions(), 0);
        assert!(process.exit_reason().is_none());
        assert!(process.mailbox().is_empty());
        assert!(process.links().is_empty());
        assert!(process.monitors().is_empty());
        assert!(process.monitored_by().is_empty());
        assert!(process.off_heap().is_empty());
        assert_eq!(process.heap_used_words(), 0);
    }

    #[test]
    fn state_flags_and_reductions_are_mutable() {
        let mut process = Process::new(pid(2)).expect("the default heap allocates");
        process.set_state(ProcessState::Waiting);
        assert_eq!(process.state(), ProcessState::Waiting);

        process.set_trap_exit(true);
        assert!(process.is_trapping_exit());
        process.flags_mut().set_system(true);
        assert!(process.flags().system());

        process.bump_reductions(5);
        process.bump_reductions(7);
        assert_eq!(process.reductions(), 12);
        process.reset_reductions();
        assert_eq!(process.reductions(), 0);

        process.set_exit_reason(make_small(9));
        assert_eq!(process.exit_reason(), Some(make_small(9)));
    }

    #[test]
    fn link_and_monitor_sets_are_reachable_through_the_pcb() {
        let mut process = Process::new(pid(3)).expect("the default heap allocates");
        assert!(process.links_mut().add(pid(4)));
        assert!(process.links().contains(pid(4)));

        let reference = RefAllocator::new().next();
        process.monitors_mut().push(Monitor {
            reference,
            target: pid(4),
            kind: MonitorKind::Process,
        });
        assert_eq!(process.monitors().find(reference).map(|m| m.target), Some(pid(4)));

        process.monitored_by_mut().push(MonitoredBy {
            reference,
            watcher: pid(5),
            kind: MonitorKind::Process,
        });
        assert_eq!(process.monitored_by().len(), 1);
        assert_eq!(process.monitored_by()[0].watcher, pid(5));

        // The outgoing port-monitor set is reachable too, and starts empty.
        assert!(process.port_monitors().is_empty());
        let port_reference = RefAllocator::new().next();
        let port = PortId::new(11);
        process.port_monitors_mut().push(PortWatch {
            reference: port_reference,
            port,
        });
        assert_eq!(process.port_monitors().len(), 1);
        assert_eq!(process.port_monitors()[0].reference, port_reference);
        assert_eq!(process.port_monitors()[0].port, port);
    }

    #[test]
    fn collect_policy_tracks_the_young_heap_capacity() {
        let process = Process::new(pid(6)).expect("the default heap allocates");
        // A fresh nursery cannot satisfy an absurd request ...
        assert!(process.should_collect(usize::MAX / 2));
        // ... but a small one does.
        assert!(!process.should_collect(1));
        assert_eq!(process.heap_used_words(), 0);
    }

    #[test]
    fn owned_off_heap_binaries_register_by_payload_and_sweep_only_declared_deaths() {
        use beam_alloc::RefcBinary;

        let mut process = Process::new(pid(30)).expect("the default heap allocates");
        assert_eq!(process.off_heap_binary_count(), 0);
        assert_eq!(process.off_heap_binary_bytes(), 0);
        assert!(process.off_heap_binary(0).is_none());

        // A registration moves the handle in and lands at slot 0; a second registration of the
        // same payload (same `data_ptr`) keeps that slot and drops the surplus handle.
        let payload = RefcBinary::new(b"a 200-byte off-heap payload ...").expect("allocate");
        let data_ptr = payload.data_ptr();
        let slot = process.register_off_heap_binary(payload.clone());
        assert_eq!(slot, 0);
        assert_eq!(process.register_off_heap_binary(payload.clone()), slot);
        assert_eq!(process.off_heap_binary_count(), 1, "one entry per payload");
        assert_eq!(
            process.off_heap_binary(slot).expect("slot 0").refcount(),
            2,
            "the local owner plus the list's single handle"
        );
        assert_eq!(process.off_heap_binary_bytes(), payload.len());
        assert_eq!(
            process.off_heap_binaries().map(|b| b.data_ptr()).collect::<Vec<_>>(),
            vec![data_ptr],
        );

        // Nothing declared dead: a sweep releases nothing (the conservative default).
        assert_eq!(process.sweep_off_heap_binaries(), 0);
        assert_eq!(process.off_heap_binary_count(), 1);

        // Declared dead: the next sweep releases it, and the slot is gone for good.
        assert!(process.declare_off_heap_binary_dead(slot));
        assert_eq!(process.sweep_off_heap_binaries(), 1);
        assert_eq!(process.off_heap_binary_count(), 0);
        assert_eq!(process.off_heap_binary_bytes(), 0);
        assert!(process.off_heap_binary(slot).is_none());
        assert_eq!(process.sweep_off_heap_binaries(), 0, "a released slot is not released again");
        assert!(!process.declare_off_heap_binary_dead(slot));

        // The payload outlives the release: the local owner still holds a handle.
        assert_eq!(payload.refcount(), 1);
        assert_eq!(payload.data_ptr(), data_ptr);
    }
    #[test]
    fn collect_reclaims_with_an_empty_mailbox_and_no_roots() {
        let mut process = Process::new(pid(7)).expect("the default heap allocates");
        // Allocate an unreferenced term (garbage), then collect: it is not rooted, so the
        // young heap comes back empty.
        process
            .heap_mut()
            .tuple(&[make_small(1), make_small(2)])
            .expect("the tuple fits");
        assert!(process.heap_used_words() > 0);

        let collection = process.collect().expect("the collection succeeds");
        assert!(collection.stats.words_reclaimed > 0);
        assert_eq!(process.heap_used_words(), 0);
        assert!(process.mailbox().is_empty());

        // `collect_if_needed` only runs when the heap cannot satisfy the request.
        assert!(!process
            .collect_if_needed(1)
            .expect("a tiny request needs no collection"));
    }

    /// [`Process::has_unpublished_roots`] is the delivery-collect gate, and it is true in exactly
    /// two cases: the per-process **relocation-exempt** flag, and a **`Running`** process state
    /// (the single-threaded scheduler's one executing process, whose live x/stack/float registers
    /// are held outside the `regs` root snapshot). Every other state — and a fresh process — is
    /// false, so the ordinary collect-and-grow delivery path is untouched.
    #[test]
    fn has_unpublished_roots_covers_the_exemption_and_a_running_process() {
        let mut process = Process::new(pid(41)).expect("the default heap allocates");
        assert_eq!(
            process.state(),
            ProcessState::Runnable,
            "a fresh process is runnable"
        );
        assert!(!process.is_relocation_exempt());
        assert!(
            !process.has_unpublished_roots(),
            "a runnable, non-exempt process's terms are all in the crate root set"
        );

        process.set_state(ProcessState::Running);
        assert!(
            process.has_unpublished_roots(),
            "the running process's live terms are outside the crate root set"
        );

        process.set_state(ProcessState::Waiting);
        assert!(
            !process.has_unpublished_roots(),
            "a waiting process's roots were published at its park"
        );

        process.set_state(ProcessState::Runnable);
        process.set_relocation_exempt(true);
        assert!(
            process.has_unpublished_roots(),
            "the exemption alone is enough (the host holds live words outside the root set)"
        );

        process.set_state(ProcessState::Running);
        process.set_relocation_exempt(false);
        assert!(
            process.has_unpublished_roots(),
            "the running state alone is enough (clearing the exemption changes nothing)"
        );
    }

    /// [`Process::grow_nursery`] enlarges the nursery **in place**: it retains the space it    /// replaces (`young_space_count` grows), runs **no** collection (`minor_gcs` is unchanged)
    /// and relocates **no** live term (a captured word keeps its exact address and value).
    ///
    /// It also pins the absence of a relocation-exempt guard: growth is legal for an exempt
    /// process (the host holding an incomplete root set), unlike [`Process::collect`] /
    /// [`Process::ensure_capacity`], and still moves nothing.
    #[test]
    fn grow_nursery_enlarges_without_collecting_or_relocating() {
        let mut process = Process::new(pid(40)).expect("the default heap allocates");

        // A live term in the young heap, captured by address.
        let term = process
            .heap_mut()
            .tuple(&[make_small(1), make_small(2)])
            .expect("the tuple fits");
        let ptr = ptr_val(term);
        // SAFETY: the tuple was just allocated in the process's young heap, so the header word at
        // `ptr` is live; the growth retains the space holding it, so the word stays readable.
        let header_before = unsafe { *ptr };

        let spaces_before = process.collector().young_space_count();
        let cap_before = process.collector().young().capacity_words();
        let minors_before = process.collector().minor_gcs();

        let grown = process
            .grow_nursery(4096)
            .expect("the default policy grows on demand");

        // (1) Growth happened: a space was retained and the current capacity grew past the request.
        assert!(process.collector().young_space_count() > spaces_before);
        assert!(process.collector().young().capacity_words() > cap_before);
        assert!(grown >= 4096, "the returned capacity covers the request");

        // (2) No collection ran.
        assert_eq!(process.collector().minor_gcs(), minors_before);

        // (3) The live term is at its EXACT same address, word for word.
        assert!(process.collector().young_contains(ptr));
        // SAFETY: as above — the retained space still holds the tuple's header.
        assert_eq!(unsafe { *ptr }, header_before);

        // (4) No exemption guard: an exempt process may grow, and still nothing moves.
        process.set_relocation_exempt(true);
        assert!(process.grow_nursery(16384).is_ok());
        assert!(process.collector().young_contains(ptr));
        assert_eq!(process.collector().minor_gcs(), minors_before);
        // SAFETY: as above — the address is unchanged across both growths.
        assert_eq!(unsafe { *ptr }, header_before);
    }

    /// A nursery request the policy's ceiling cannot reach yields the typed
    /// [`BeamError::Alloc`] heap-exhaustion error (propagated unchanged through `?`), leaving the
    /// collector untouched.
    #[test]
    fn grow_nursery_returns_the_typed_heap_exhaustion_error_under_a_cap() {
        let mut process = Process::with_policy(pid(41), HeapPolicy::from_min_max(0, 512))
            .expect("the capped heap allocates");
        let err = process
            .grow_nursery(1_000_000)
            .expect_err("the ceiling makes the request unreachable");
        assert!(matches!(err, BeamError::Alloc(_)));
    }

    /// Pins the mailbox half of the root walk: every queued message contributes **three**
    /// envelope words — `[payload, from, token]` — to the flat root slice, and all three are
    /// written back into the re-installed message.
    ///
    /// The test captures the `roots` slice handed to the collection action (the only way to
    /// observe it from outside: `run_rooted_collection` is private, so an integration test under
    /// `tests/` cannot reach it) and asserts the documented per-message order and adjacency. A
    /// walk that gathered only the payloads leaves the `from`/`token` words out of the slice, so
    /// it fails here.
    ///
    /// The envelope words must be **immediates**: `beam_message::Queue::enqueue`'s debug tripwire
    /// rejects a non-immediate `from`/`token` (`debug_assert_enqueue_terms`), and this crate may
    /// not edit `beam_message` — so a heap-term envelope cannot be enqueued from here (and must
    /// not be forced past the tripwire). The values are still distinct per message, which is what
    /// makes the presence *and* the order of each word observable.
    #[test]
    fn run_rooted_collection_gathers_and_scatters_the_message_envelope_words() {
        let mut process = Process::new(pid(31)).expect("the default heap allocates");
        let (first_payload, first_from, first_token) =
            (make_small(41), make_small(7), make_small(8));
        let (second_payload, second_from, second_token) =
            (make_small(42), make_small(9), make_small(10));
        process
            .mailbox_mut()
            .enqueue(Message::new(first_payload, first_from).with_token(first_token));
        process
            .mailbox_mut()
            .enqueue(Message::new(second_payload, second_from).with_token(second_token));

        // Capture the root slice the collection action is handed, then run a real collection so
        // the walk's scatter step actually executes.
        let mut captured: Vec<Eterm> = Vec::new();
        process
            .run_rooted_collection(|collector, roots| {
                captured = roots.to_vec();
                collector.collect(roots)
            })
            .expect("the collection succeeds");

        // Every envelope word is gathered, in `[payload, from, token]` order and adjacent within
        // its message's three-word slice.
        let base = captured
            .iter()
            .position(|word| *word == first_payload)
            .expect("the first message's payload is a root");
        assert_eq!(captured[base + MESSAGE_ROOT_PAYLOAD], first_payload);
        assert_eq!(captured[base + MESSAGE_ROOT_FROM], first_from);
        assert_eq!(captured[base + MESSAGE_ROOT_TOKEN], first_token);

        let base = base + MESSAGE_ROOT_WORDS;
        assert_eq!(captured[base + MESSAGE_ROOT_PAYLOAD], second_payload);
        assert_eq!(captured[base + MESSAGE_ROOT_FROM], second_from);
        assert_eq!(captured[base + MESSAGE_ROOT_TOKEN], second_token);

        // The scatter step wrote the words back into the re-installed envelopes, keeping FIFO.
        let envelope: Vec<(Eterm, Eterm, Eterm)> = process
            .mailbox()
            .iter()
            .map(|message| (message.payload(), message.from(), message.token()))
            .collect();
        assert_eq!(
            envelope,
            vec![
                (first_payload, first_from, first_token),
                (second_payload, second_from, second_token),
            ]
        );
    }

    /// Pins the host-published extra-root half of the root walk: a term a host holds **outside**
    /// `x`/`stack` ([`Registers::extra_roots`] — a parked engine machine's live pending-exception
    /// reason, say) is gathered **after** the register and stack words and written back relocated,
    /// so a delivery-driven collection cannot leave the host holding a recycled word.
    #[test]
    fn run_rooted_collection_gathers_and_scatters_the_extra_roots() {
        let mut process = Process::new(pid(51)).expect("the default heap allocates");
        process
            .registers_mut()
            .x_mut()
            .set_live(1)
            .expect("1 <= MAX_REG");
        process
            .registers_mut()
            .x_mut()
            .set(0, make_small(1))
            .expect("0 < live");
        let stack_term = process
            .heap_mut()
            .cons(make_small(2), NIL)
            .expect("the stack cons fits");
        process.registers_mut().stack_mut().push(stack_term);
        let extra = process
            .heap_mut()
            .tuple(&[make_small(3), make_small(4)])
            .expect("the extra-root tuple fits");
        process.registers_mut().extra_roots_mut().push(extra);

        // Capture the pre-collection slice, then run a real collection so the write-back runs.
        let mut captured: Vec<Eterm> = Vec::new();
        process
            .run_rooted_collection(|collector, roots| {
                captured = roots.to_vec();
                collector.collect(roots)
            })
            .expect("the collection succeeds");

        // Order: the live x-register, then the stack word, then the host-published extra root.
        assert_eq!(captured[0], make_small(1));
        assert_eq!(captured[1], stack_term);
        assert_eq!(captured[2], extra);
        assert_eq!(captured.len(), 3);

        // The host's word was written back relocated into a live space and is intact.
        let extra_after = *process
            .registers()
            .extra_roots()
            .first()
            .expect("the extra root survived the collection");
        let ptr = ptr_val(extra_after);
        assert!(
            process.collector().young_contains(ptr) || process.collector().mature_contains(ptr),
            "the extra root was relocated into the surviving young/mature space"
        );
        // SAFETY: the assertion above proved `extra_after` points into a live space and it is the
        // 2-tuple allocated above, so slots 0..2 are in bounds.
        unsafe {
            assert_eq!(tuple_size(extra_after), 2);
            assert_eq!(tuple_element(extra_after, 0), make_small(3));
            assert_eq!(tuple_element(extra_after, 1), make_small(4));
        }
    }

    #[test]
    fn deliver_simple_enqueues_an_immediate_message() {
        let mut process = Process::new(pid(8)).expect("the default heap allocates");
        let from = pid(9).to_eterm();
        process
            .deliver_simple(from, make_small(41))
            .expect("an immediate always fits");
        assert_eq!(process.mailbox().len(), 1);

        let message = process
            .receive(|m| m.payload() == make_small(41))
            .expect("the queued message matches");
        assert_eq!(message.from(), from);
        assert_eq!(message.token(), NIL);
        assert!(process.mailbox().is_empty());
        assert!(process.receive(|_| true).is_none(), "the mailbox is empty again");
    }

    #[test]
    fn signal_atoms_are_usable_from_a_plain_table() {
        // Guards the `SignalAtoms` seam the table depends on (the atom indices are stable
        // for a freshly built table).
        let mut table = AtomTable::new();
        let atoms = SignalAtoms::from_table(&mut table);
        assert_eq!(table.name(atoms.exit()), Some("EXIT"));
        assert_eq!(table.name(atoms.kill()), Some("kill"));
    }

    /// The per-object reachability test keys on the object's **header word** alone: the `BinRef` of
    /// the frozen refc pair is identified (and its `val` word read as the payload pointer), while the
    /// pair's `ErlSubBits` root and an unrelated boxed shape name no off-heap payload.
    ///
    /// A [`beam_gc::ReachableObject`] view cannot be built outside `beam_gc`, so the discrimination is
    /// driven through a **real walk** — [`Process::sweep_off_heap_binaries_reachable`] over the
    /// process's own rooted young heap — and read off which registered payload survives:
    ///
    /// * a reachable `BinRef` (the one behind the rooted pair) ⇒ its payload is **retained**;
    /// * the rooted term is the pair's `ErlSubBits`, not its `BinRef`: only the walk's descent of the
    ///   `ErlSubBits`'s `orig` slot makes the `BinRef` an object of the walk at all, so an
    ///   identification that found no `BinRef` behind an `ErlSubBits` root would release the payload
    ///   (a dangle) and the retention assertion below would fail;
    /// * an unrelated boxed shape (a tuple) is not a handle either, even when its word 1 spells a
    ///   real payload address, so the payload only that spoofed word names is still reclaimed.
    #[test]
    fn the_reachability_test_identifies_a_bin_ref_by_its_header() -> Result<()> {
        use beam_term::prelude::{write_refc_binary, ERL_REFC_BITS_SIZE};

        // The payload the rooted pair names, and one that no on-heap handle names.
        let named = RefcBinary::new(b"a payload named by the rooted refc pair").expect("allocate");
        let named_ptr = named.data_ptr();
        let unrooted = RefcBinary::new(b"a payload no on-heap handle names").expect("allocate");
        let unrooted_ptr = unrooted.data_ptr();

        let mut process = Process::new(pid(31)).expect("the default heap allocates");
        let named_slot = process.register_off_heap_binary(named.clone());
        let unrooted_slot = process.register_off_heap_binary(unrooted);

        // The frozen `[BinRef(3)][ErlSubBits(5)]` pair, written on the process's **own young heap**;
        // the returned root — the `ErlSubBits` — is the term a caller roots.
        // SAFETY: `alloc_words` hands back `ERL_REFC_BITS_SIZE` exclusively-owned, writable words of
        // the process's young heap; `named_ptr` is the live, word-aligned first data byte of the
        // payload the registered `named` handle keeps alive, and `0..named.len() * 8` covers exactly
        // that payload.
        let pair = unsafe {
            write_refc_binary(
                process.heap_mut().alloc_words(ERL_REFC_BITS_SIZE)?,
                named_ptr as *mut Eterm,
                core::ptr::null_mut(),
                named_ptr,
                0,
                named.len() * 8,
            )
        };
        // An unrelated boxed shape: the walk reports it as an object too, and its header — not the
        // word below it — decides what it is. Its word 1 deliberately spells the **unrooted**
        // payload's address (a bare word whose primary tag is the frozen non-value `HEADER` tag, so
        // the walk itself ignores it): an identification that dropped the `HEADER_BIN_REF` test and
        // read word 1 of *every* boxed object would read this word as a payload pointer and retain
        // that payload, so the reclaim assertion below has teeth.
        let unrelated = process
            .heap_mut()
            .tuple(&[Eterm::from_raw(unrooted_ptr as usize), make_small(0)])?;

        process.registers_mut().x_mut().set_live(2)?;
        process.registers_mut().x_mut().set(0, pair)?;
        process.registers_mut().x_mut().set(1, unrelated)?;

        assert_eq!(
            process.sweep_off_heap_binaries_reachable()?,
            1,
            "the pair's `BinRef` names its payload (retained); the payload no handle names is reclaimed"
        );
        let held = process
            .off_heap_binary(named_slot)
            .expect("the `BinRef` behind the rooted `ErlSubBits` named its payload");
        assert_eq!(held.data_ptr(), named_ptr, "the payload pointer is the `BinRef.val` word");
        assert_eq!(held.as_bytes(), named.as_bytes(), "the payload bytes are untouched");
        assert!(
            process.off_heap_binary(unrooted_slot).is_none(),
            "an unrelated boxed shape is not an off-heap handle, so its spoofed payload word retained nothing"
        );
        Ok(())
    }

    /// Registration is **O(1) amortized**: its work is independent of how many slots the owned list
    /// has ever handed out (live *or* released), so re-registering a set of payloads costs exactly
    /// what registering them the first time cost.
    ///
    /// The `registration_probes` counter records the **per-call work of one `data_ptr → slot`
    /// resolution** inside [`Process::register_off_heap_binary`] — in the index path exactly one
    /// unit per call, because a resolution is a single keyed lookup in
    /// `Process::owned_binary_index` and therefore considers exactly one slot candidate. The
    /// **old** implementation resolved through `owned_binary_slot`'s
    /// `(0..owned_binary_slots).find(...)`, whose per-call work is one candidate examined per slot
    /// in the space, so the honest per-candidate count for a call over `S` slots (`S` = that call's
    /// pre-insert mirror: the slot space grows monotonically with successful registrations) is
    /// `O(S)`. Counting the old code that way would make batch 1 (each call a miss over an
    /// ever-larger space) probe `0 + 1 + … + (N-1)` units and batch 2 — all `N` slots live — probe
    /// an `O(N)` prefix per call (≈`N²/2`), so `batch2` would be far larger than `batch1`; that is
    /// exactly the inequality the `batch2 == batch1` assertion rejects. With the index every call
    /// probes exactly once, so both batches probe `N`.
    #[test]
    fn off_heap_binary_registration_work_is_independent_of_slot_count() {
        const N: usize = 1000;
        let mut process = Process::new(pid(41)).expect("the default heap allocates");
        // One stable handle per payload keeps every payload alive across both batches, so no address
        // is freed and reused (a reused address would alias two distinct "payloads") and the process
        // slot is only one of the payload's two owners.
        let payloads: Vec<RefcBinary> = (0..N)
            .map(|i| RefcBinary::new(&[i as u8; 8]).expect("allocate"))
            .collect();

        // Batch 1: every payload is new — each call is a miss followed by a fresh insert.
        let before = registration_probes();
        for binary in &payloads {
            process.register_off_heap_binary(binary.clone());
        }
        let batch1 = registration_probes() - before;
        assert_eq!(batch1, N as u64, "one probe per registration call — no scan");
        assert_eq!(process.off_heap_binary_count(), N, "every distinct payload got a slot");

        // Batch 2: the same payloads are re-registered; each is a dedup hit, still one probe. If
        // resolution scanned the slot space, this batch would probe O(N) candidates per call and the
        // two counts could not be equal.
        let before = registration_probes();
        for binary in &payloads {
            process.register_off_heap_binary(binary.clone());
        }
        let batch2 = registration_probes() - before;
        assert_eq!(
            batch2, batch1,
            "re-registering N payloads over N live slots costs the same as the first registration"
        );
    }

    /// A swept-released slot is pruned from the live index, so it never resolves again and
    /// re-registering the same payload takes a **fresh** slot.
    #[test]
    fn off_heap_binary_release_prunes_the_index_and_a_fresh_registration_gets_a_new_slot() -> Result<()>
    {
        let mut process = Process::new(pid(42)).expect("the default heap allocates");
        // A stable handle keeps the payload alive after the process's slot is released, so its
        // address stays valid and cannot be silently reused by a later allocation.
        let payload = RefcBinary::new(b"release-cleanup").expect("allocate");
        let data_ptr = payload.data_ptr();

        let released_slot = process.register_off_heap_binary(payload.clone());
        assert_eq!(
            process.off_heap_binary(released_slot).map(RefcBinary::data_ptr),
            Some(data_ptr),
            "the payload is registered at its slot"
        );

        // Declare it dead and sweep: the release funnel must prune the index for it.
        assert!(process.declare_off_heap_binary_dead(released_slot));
        assert_eq!(
            process.sweep_off_heap_binaries(),
            1,
            "the declared-dead slot is released"
        );
        assert!(
            process.off_heap_binary(released_slot).is_none(),
            "a released slot no longer resolves"
        );

        // Re-registering the same payload — still alive through the caller's handle — gets a fresh
        // slot, never the released one: proof the index no longer maps `data_ptr` to the old slot.
        let fresh_slot = process.register_off_heap_binary(payload.clone());
        assert_ne!(fresh_slot, released_slot, "a released slot is never reused");
        assert_eq!(
            process.off_heap_binary(fresh_slot).map(RefcBinary::data_ptr),
            Some(data_ptr),
            "the fresh slot names the same payload"
        );
        assert!(process.off_heap_binary(released_slot).is_none());
        Ok(())
    }
}
