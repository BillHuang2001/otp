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

//! The process/port table: pid allocation, the local name registry and the signal hub.
//!
//! [`ProcessTable`] is the **single place that mutates two or more processes at once** —
//! links, monitors and exit propagation inherently touch pairs of PCBs, and holding two
//! `&mut Process` out of one `HashMap` is a borrow-checker impossibility. Every such
//! operation therefore lives here and uses a **collect-then-apply worklist**: gather the
//! link targets / watchers, then mutate each target transiently. A [`Process`] itself
//! holds only its own sets (see the crate `CONTEXT.md`, Design Decisions).
//!
//! # Decoupled, ordered signals
//!
//! Exit/link/monitor signals are **never applied where they are generated**. A signalling
//! operation *enqueues* a [`PendingSignal`] on the receiver's own
//! [`SignalQueue`](crate::signal_queue::SignalQueue) — copying the reason into the receiver's
//! heap so it survives the signaller's death — and the signal is applied later:
//!
//! * **at the receiver's schedule-in**, through [`ProcessTable::service_signals`] (the
//!   scheduler calls it on the dequeued pid before running it); and
//! * **eagerly at the end of the sending operation**, for a receiver that is **not** the
//!   currently executing process (ERTS's early signal handling). "Currently executing" is
//!   [`ProcessState::Running`]: the single scheduler sets it before a runner runs, so at most
//!   one process is `Running` at a time, and a signal to that process is deferred to its next
//!   schedule-in. This choice is what preserves the observable end state of a synchronous
//!   propagation (a signal to a blocked/idle peer is still applied before the caller returns).
//!
//! Servicing drains a receiver's queue **in FIFO order** and applies each signal through
//! [`exit_policy`] (terminate / trap as `{'EXIT', …}` / ignore) or builds the `{'DOWN', …}`
//! message. A termination cascades through an explicit worklist (the pending-service queue),
//! never through recursion, so a link cycle (A↔B) terminates in a bounded number of steps.
//! [`crate::signal_queue`] states the exact ordering guarantee (and its limits).
//!
//! A local **link exit** and a local **monitor-DOWN** go through the queue. A **remote**
//! (external-pid) or **port** signal does not: it would carry no `trap_exit` policy decision
//! to defer, so `node_down`/`port_closed` materialise it straight into the mailbox.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_ptab.{c,h}` (the process/port
//! table and the local name registry), `erl_process.c` (`erts_ptab_new_process`) and
//! `erl_proc_sig_queue.c` (the signal queue this module reproduces).

use std::collections::{HashMap, VecDeque};

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_gc::HeapPolicy;
use beam_message::{Message, MsoHandle, OffHeapTable};
use beam_term::prelude::{atom_val, is_atom, is_immediate, make_atom, Atom, Eterm};

use crate::error;
use crate::exit_observer::{ProcessExit, ProcessExitObserver};
use crate::monitor::{
    payload_is_down_for, Monitor, MonitorKind, MonitorRef, MonitoredBy, PortWatch, RefAllocator,
};
use crate::pid::{PortId, ProcessId, MAX_PROCESS_NUMBER};
use crate::port::PortTable;
use crate::process::{DeliverOutcome, Process, ProcessState};
use crate::remote::{DistSignal, DistSignalRouter, PidTarget, RemoteMonitor, RemotePid};
use crate::run_queue::Priority;
use crate::signal::{
    build_down_message_port, build_down_message_remote, build_down_message_term,
    build_exit_message, build_exit_message_term, exit_policy, ExitPolicy, ExitSignal, SignalAtoms,
};
use crate::signal_queue::PendingSignal;

/// The outcome of applying an exit signal to a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExitEffect {
    /// The target was terminated.
    Terminated,
    /// The target trapped the signal and received `{'EXIT', From, Reason}`.
    Trapped,
    /// The signal was ignored (a `normal` signal to a non-trapping process).
    Ignored,
}

/// One process-table slot: its current serial and the process it holds.
///
/// A slot is reused with a **bumped serial**, so a stale pid never resolves to the new
/// occupant (see [`crate::pid`]).
struct Slot {
    serial: u64,
    process: Option<Process>,
}

/// The target descriptor of a **named** monitor (`erlang:monitor(process, Name)`).
///
/// `Monitor`/`MonitoredBy` carry a [`ProcessId`] target only, and their shape is fixed (built
/// by struct literal), so the `{Name, LocalNode}` target a named monitor reports in its
/// `{'DOWN', …}` is kept beside them, keyed by the monitor reference, in
/// [`ProcessTable::named_monitors`].
///
/// Both fields are **atoms — immediates** — so this record introduces no GC rootset and never
/// holds a term that could be relocated by a collection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct NamedTarget {
    /// The registered name that was monitored.
    name: Atom,
    /// The node the name was resolved on (the local node, spelled into the DOWN target).
    node: Atom,
}

/// Borrows the live process named by `pid`, verifying the slot serial first.
fn lookup(slots: &[Slot], pid: ProcessId) -> Option<&Process> {
    let slot = slots.get(pid.number() as usize)?;
    if slot.serial != pid.serial() {
        return None;
    }
    slot.process.as_ref()
}

/// Borrows the live process named by `pid` mutably, verifying the slot serial first.
fn lookup_mut(slots: &mut [Slot], pid: ProcessId) -> Option<&mut Process> {
    let slot = slots.get_mut(pid.number() as usize)?;
    if slot.serial != pid.serial() {
        return None;
    }
    slot.process.as_mut()
}

/// Builds a signal term — together with the handles of the off-heap objects it owns — on
/// `process`'s heap, collecting once and retrying when the heap is exhausted (a full heap is a
/// normal condition, not fatal OOM).
///
/// `build` returns whatever the caller needs (the term plus the handles of the fresh off-heap
/// table it copied into, or the term plus extra terms materialised on the same heap). It creates
/// that table itself, so a retry always starts from a clean table and never reuses a stale handle.
fn build_signal_term<T, F>(process: &mut Process, build: F) -> Result<T>
where
    F: Fn(&mut Heap) -> Result<T>,
{
    match build(process.heap_mut()) {
        Ok(built) => Ok(built),
        Err(BeamError::Alloc(_)) => {
            process.collect()?;
            build(process.heap_mut())
        }
        Err(error) => Err(error),
    }
}

/// Copies `reason` — whose heap is described by `src_off_heap` — into `process`'s own heap,
/// registering every referenced off-heap object in the process's own off-heap table, and
/// returns the copied term. Collects once and retries on heap exhaustion.
///
/// `None` means the reason is already owned by `process` (a process signalling itself) and is
/// returned unchanged.
fn copy_reason_onto(
    process: &mut Process,
    reason: Eterm,
    src_off_heap: Option<&OffHeapTable>,
) -> Result<Eterm> {
    let Some(src) = src_off_heap else {
        return Ok(reason);
    };
    match process.copy_into_heap(reason, src) {
        Ok(copied) => Ok(copied),
        Err(BeamError::Alloc(_)) => {
            process.collect()?;
            process.copy_into_heap(reason, src)
        }
        Err(error) => Err(error),
    }
}

/// Validates an `erlang:monitor/3` `{tag, Tag}` user tag.
///
/// `None` (no option) and an **immediate** tag pass through unchanged; anything else — a term
/// resident in a per-process heap, which a monitor record (a plain value the collector never
/// scans) could not keep valid across a collection — is refused with the typed
/// [`error::unsupported_tag`], so the caller never gets a dangling tag in a `{'DOWN', …}`.
fn check_tag(tag: Option<Eterm>) -> Result<Option<Eterm>> {
    match tag {
        None => Ok(None),
        Some(term) if is_immediate(term) => Ok(Some(term)),
        Some(_) => Err(error::unsupported_tag()),
    }
}

/// Remembers `reference`'s `erlang:monitor/3` user tag, if it has one.
///
/// Only a supplied tag is stored — an untagged monitor leaves no entry, so a lookup answering
/// `None` means the default `'DOWN'` head atom (see `monitor.rs`'s user-tag contract). The
/// stored term is a validated immediate.
fn remember_tag(tags: &mut HashMap<MonitorRef, Eterm>, reference: MonitorRef, tag: Option<Eterm>) {
    if let Some(tag) = tag {
        tags.insert(reference, tag);
    }
}

/// Forgets `reference`'s user tag — its monitor is gone, so the entry must not linger.
fn forget_tag(tags: &mut HashMap<MonitorRef, Eterm>, reference: MonitorRef) {
    tags.remove(&reference);
}

/// A standalone copy of `src`'s off-heap handles (each cloned — the refcount is bumped), with
/// the **same slot indices** so a term copied against the snapshot resolves exactly as it would
/// against `src`.
///
/// Used when the copy source lives in the table and the copy destination is mutated through the
/// same `&mut ProcessTable`: the snapshot breaks the aliasing so
/// [`Process::copy_into_heap`](crate::process::Process::copy_into_heap) can borrow the
/// destination. The cloned handles are released when the snapshot drops, so the refcounts net
/// out.
fn snapshot_off_heap(src: &OffHeapTable) -> OffHeapTable {
    let mut snapshot = OffHeapTable::new();
    for handle in src.iter() {
        snapshot.push(handle.clone());
    }
    snapshot
}

/// Builds `{'EXIT', from, reason}` on `recipient`'s heap and enqueues it as a message.
///
/// `reason` is already owned by `recipient`'s heap: it was copied in when the signal was
/// enqueued (`enqueue_exit_signal`), so the delivered reason always resides in the recipient's
/// own heap. This deliberately does **not** collect on heap exhaustion — a collection would
/// relocate `reason`, turning the raw word stale — so a full heap surfaces as
/// `BeamError::Alloc` and the caller retries after a collection with a freshly read reason
/// (see [`ProcessTable::deliver_head_signal`]).
///
/// # Errors
///
/// Propagates `BeamError::Alloc` when the message does not fit.
fn enqueue_exit_on(
    recipient: &mut Process,
    atoms: &SignalAtoms,
    from: ProcessId,
    reason: Eterm,
) -> Result<()> {
    let term = build_exit_message(recipient.heap_mut(), atoms, from, reason)?;
    recipient
        .mailbox_mut()
        .enqueue(Message::new(term, from.to_eterm()));
    Ok(())
}

/// The process table, the local name registry and the signal/link/monitor hub.
pub struct ProcessTable {
    /// The numbered slots; a `None` process is a free slot awaiting reuse.
    slots: Vec<Slot>,
    /// Free slot numbers available for reuse (with a bumped serial).
    free: Vec<u64>,
    /// The local name registry (`erlang:register/2` / `whereis/1`).
    names: HashMap<Atom, ProcessId>,
    /// The never-reusing monitor-reference allocator.
    refs: RefAllocator,
    /// The well-known signal atoms.
    atoms: SignalAtoms,
    /// The **wake list**: the pids of blocked processes that a delivery/signal has just made
    /// runnable, so the single-threaded scheduler can absorb them (see
    /// [`ProcessTable::drain_woken`] and `crate::scheduler::Scheduler::resume_woken`).
    ready: Vec<ProcessId>,
    /// Per local process: its remote (external-node) link partners.
    remote_links: HashMap<ProcessId, Vec<RemotePid>>,
    /// Per local process: its outgoing monitors on remote processes/ports.
    remote_monitors: HashMap<ProcessId, Vec<RemoteMonitor>>,
    /// The target descriptor of every **named** monitor (`erlang:monitor(process, Name)`), keyed
    /// by its reference (see [`NamedTarget`]). An entry dies with its monitor: `demonitor`, the
    /// dying holder's outgoing-monitor sweep and a delivered DOWN all remove it.
    named_monitors: HashMap<MonitorRef, NamedTarget>,
    /// The `erlang:monitor/3` `{tag, Tag}` **user tag** of every tagged monitor, keyed by its
    /// reference.
    ///
    /// A tag cannot be a field of [`Monitor`]/[`MonitoredBy`]/[`PortWatch`] — those are built by
    /// struct literal from other crates, so their shape is fixed — and it is kept here instead,
    /// exactly like [`NamedTarget`] (see `monitor.rs`'s user-tag contract). An entry is created
    /// only when a tag is supplied, holds a **heap-independent immediate** (so it is not a GC
    /// root and never needs relocating), and is removed the moment its monitor dies —
    /// `demonitor`/`demonitor_flush`, the terminated target's watcher sweep, `demonitor_port` and
    /// `port_closed` — so it cannot leak.
    tags: HashMap<MonitorRef, Eterm>,
    /// The atom naming the **local node**, used to spell the `{Name, LocalNode}` target of a
    /// named monitor. `None` until [`ProcessTable::set_local_node`] installs it; a named monitor
    /// requested without it is a typed [`error::unsupported`] rather than a wrong target.
    local_node: Option<Atom>,
    /// The distribution seam (see [`DistSignalRouter`]); `None` means **record-only** — the
    /// local relationship is still tracked, only the wire signal is dropped (a node with no
    /// distribution configured).
    router: Option<Box<dyn DistSignalRouter>>,
    /// The process-death observer seam (see [`ProcessExitObserver`]); `None` means no observer is
    /// installed and termination behaves exactly as before the seam existed. Invoked **exactly
    /// once per death** by [`ProcessTable::terminate_into`], before the dying heap is released.
    exit_observer: Option<Box<dyn ProcessExitObserver>>,
}

impl ProcessTable {
    /// An empty table using `atoms` to build signal messages.
    pub fn new(atoms: SignalAtoms) -> ProcessTable {
        ProcessTable {
            slots: Vec::new(),
            free: Vec::new(),
            names: HashMap::new(),
            refs: RefAllocator::new(),
            atoms,
            ready: Vec::new(),
            remote_links: HashMap::new(),
            remote_monitors: HashMap::new(),
            named_monitors: HashMap::new(),
            tags: HashMap::new(),
            local_node: None,
            router: None,
            exit_observer: None,
        }
    }    // --- allocation / lookup -------------------------------------------------

    /// Allocates a fresh pid, reusing a free slot with a bumped serial.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::SystemLimit` when the pid number space is exhausted.
    pub fn alloc_pid(&mut self) -> Result<ProcessId> {
        let number = match self.free.pop() {
            Some(number) => number,
            None => {
                // Grow the table: a number that was never handed out yet. The slot's
                // serial is 0 for a brand-new number and already bumped for a reused one.
                let number = self.slots.len() as u64;
                if number > MAX_PROCESS_NUMBER {
                    return Err(error::system_limit());
                }
                self.slots.push(Slot {
                    serial: 0,
                    process: None,
                });
                number
            }
        };
        let serial = self.slots[number as usize].serial;
        Ok(ProcessId::new(number, serial))
    }

    /// Allocates a pid and installs a fresh process with `policy`.
    ///
    /// # Errors
    ///
    /// Propagates pid allocation and initial heap allocation failures.
    pub fn spawn(&mut self, policy: HeapPolicy) -> Result<ProcessId> {
        let pid = self.alloc_pid()?;
        let process = Process::with_policy(pid, policy)?;
        self.insert(process)?;
        Ok(pid)
    }

    /// Allocates a pid and installs a fresh process with the default heap policy.
    ///
    /// # Errors
    ///
    /// Propagates pid allocation and initial heap allocation failures.
    pub fn spawn_default(&mut self) -> Result<ProcessId> {
        self.spawn(HeapPolicy::default())
    }

    /// Installs `process` in the slot its pid names.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::SystemLimit` when the pid's number is out of range.
    pub fn insert(&mut self, process: Process) -> Result<()> {
        let pid = process.pid();
        let number = pid.number();
        if number > MAX_PROCESS_NUMBER {
            return Err(error::system_limit());
        }
        let index = number as usize;
        // A pid built by hand may point past the current table: materialise the gap.
        while self.slots.len() <= index {
            self.slots.push(Slot {
                serial: 0,
                process: None,
            });
        }
        // The slot is no longer available for reuse.
        self.free.retain(|&free| free != number);
        let slot = &mut self.slots[index];
        slot.serial = pid.serial();
        slot.process = Some(process);
        Ok(())
    }

    /// Borrows the process named by `pid`.
    pub fn get(&self, pid: ProcessId) -> Option<&Process> {
        lookup(&self.slots, pid)
    }

    /// Borrows the process named by `pid`, mutably.
    pub fn get_mut(&mut self, pid: ProcessId) -> Option<&mut Process> {
        lookup_mut(&mut self.slots, pid)
    }

    /// Removes the process named by `pid`, leaving the slot free for reuse.
    pub fn take(&mut self, pid: ProcessId) -> Option<Process> {
        let slot = self.slots.get_mut(pid.number() as usize)?;
        if slot.serial != pid.serial() {
            return None;
        }
        let process = slot.process.take()?;
        // A reused slot names a *different* process: bump the serial so every pid that
        // used to name this slot goes stale for good.
        slot.serial = slot.serial.wrapping_add(1);
        self.free.push(pid.number());
        // A pid that is no longer live cannot be resumed: drop any pending wake for it.
        self.ready.retain(|&woken| woken != pid);
        Some(process)
    }
    /// Removes the process named by `pid`.
    pub fn remove(&mut self, pid: ProcessId) -> Option<Process> {
        self.take(pid)
    }

    /// Whether `pid` names a live process.
    pub fn contains(&self, pid: ProcessId) -> bool {
        lookup(&self.slots, pid).is_some()
    }

    /// Whether `pid` names a live process (`erlang:is_process_alive/1`).
    ///
    /// The liveness test for the `spawn*`/`monitor`/`send` BIFs; it is exactly
    /// [`ProcessTable::contains`], named for the observable it answers.
    pub fn is_process_alive(&self, pid: ProcessId) -> bool {
        self.contains(pid)
    }

    /// The termination reason recorded on `pid`'s PCB, if it has one.
    ///
    /// `Ok(None)` means the process is live and has not terminated yet. This is the source for
    /// `erlang:process_info/2`'s exit-reason item and for the reason a linked/monitoring peer is
    /// told.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `pid` is unknown (or stale).
    pub fn exit_reason(&self, pid: ProcessId) -> Result<Option<Eterm>> {
        self.get(pid)
            .map(Process::exit_reason)
            .ok_or_else(error::unknown_process)
    }

    /// The number of live processes.
    pub fn len(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.process.is_some())
            .count()
    }

    /// Iterates the live pids.
    pub fn pids(&self) -> impl Iterator<Item = ProcessId> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.process.is_some())
            .map(|(number, slot)| ProcessId::new(number as u64, slot.serial))
    }

    /// Sets the scheduling priority of the process named by `pid`
    /// (`erlang:process_flag(priority, P)`).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `pid` is unknown.
    pub fn set_priority(&mut self, pid: ProcessId, priority: Priority) -> Result<()> {
        self.get_mut(pid)
            .ok_or_else(error::unknown_process)
            .map(|process| process.set_priority(priority))
    }

    // --- GC relocation exemption ---------------------------------------------

    /// Marks the process named by `pid` as **relocation-exempt** (or clears the mark).
    ///
    /// A thin table-level convenience over
    /// [`Process::set_relocation_exempt`](crate::process::Process::set_relocation_exempt): the host
    /// (`beam_rt`) marks a process whose full root set it cannot publish, so the crate never
    /// relocates its heap (see that method for the full contract). The table's own `deliver`
    /// already propagates [`Process::deliver`](crate::process::Process::deliver)'s error unchanged,
    /// so no table-level delivery change is needed.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `pid` is unknown.
    pub fn set_relocation_exempt(&mut self, pid: ProcessId, exempt: bool) -> Result<()> {
        self.get_mut(pid)
            .ok_or_else(error::unknown_process)
            .map(|process| process.set_relocation_exempt(exempt))
    }

    /// Whether the process named by `pid` is **relocation-exempt** (default `false`).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `pid` is unknown.
    pub fn is_relocation_exempt(&self, pid: ProcessId) -> Result<bool> {
        self.get(pid)
            .map(Process::is_relocation_exempt)
            .ok_or_else(error::unknown_process)
    }
    // --- message delivery ----------------------------------------------------

    /// Delivers `term` (with off-heap references resolved against `src_off_heap`) from `from`
    /// into `to`'s mailbox, waking `to` when it was blocked.
    ///
    /// This is the table-level entry point a runner uses to send between processes: the wake
    /// is recorded on the table's wake list and absorbed by
    /// `crate::scheduler::Scheduler::resume_woken` at the end of the current slice, so a send
    /// during a slice never lets `run_ready` exit with a freshly-woken process still queued.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `to` is unknown; propagates the copy engine's
    /// `BeamError::Alloc` when the message still does not fit after a collection.
    pub fn deliver(
        &mut self,
        to: ProcessId,
        from: Eterm,
        term: Eterm,
        src_off_heap: &OffHeapTable,
    ) -> Result<DeliverOutcome> {
        let outcome = {
            let Some(recipient) = lookup_mut(&mut self.slots, to) else {
                return Err(error::unknown_process());
            };
            recipient.deliver(from, term, src_off_heap)?
        };
        if outcome.woke() {
            self.ready.push(to);
        }
        Ok(outcome)
    }

    /// The no-off-heap convenience form of [`ProcessTable::deliver`].
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `to` is unknown; propagates the copy engine's
    /// `BeamError::Alloc` when the message still does not fit after a collection.
    pub fn deliver_simple(
        &mut self,
        to: ProcessId,
        from: Eterm,
        term: Eterm,
    ) -> Result<DeliverOutcome> {
        let no_off_heap = OffHeapTable::new();
        self.deliver(to, from, term, &no_off_heap)
    }

    /// Returns — and clears — the pids of the blocked processes a delivery/signal has just
    /// made runnable, in the order they were woken.
    ///
    /// `crate::scheduler::Scheduler::resume_woken` is the consumer; callers that send from
    /// **outside** a scheduler slice call it (directly or via the scheduler) to learn which
    /// processes to enqueue.
    pub fn drain_woken(&mut self) -> Vec<ProcessId> {
        core::mem::take(&mut self.ready)
    }

    /// The pids of the blocked processes a delivery/signal has just made runnable, without
    /// clearing the wake list (see [`ProcessTable::drain_woken`]).
    pub fn woken(&self) -> &[ProcessId] {
        &self.ready
    }

    // --- signals / links / monitors -----------------------------------------
    /// Sends an exit signal to `target`, following [`crate::signal::exit_policy`].
    ///
    /// The signal is **enqueued** on the target's [`SignalQueue`](crate::signal_queue) — never
    /// applied inline (the reason is copied into the target's own heap, so it outlives the
    /// signaller) — and then serviced **eagerly** unless the target is the currently executing
    /// process ([`ProcessState::Running`]), whose signal is deferred to its next schedule-in
    /// (see [`ProcessTable::service_signals`]).
    ///
    /// # Return value for a deferred signal
    ///
    /// The returned [`ExitEffect`] is the effect [`exit_policy`] predicts **now**: for an
    /// eagerly applied signal it is the observed outcome; for a deferred one it is the
    /// prediction (what will happen when the target next runs).
    ///
    /// # Errors
    ///
    /// Propagates the signalling/termination errors (`BeamError::TermNotFound` for an
    /// unknown target, `BeamError::Alloc` for a trapped message that does not fit).
    pub fn apply_exit_signal(
        &mut self,
        target: ProcessId,
        signal: ExitSignal,
    ) -> Result<ExitEffect> {
        let Some(process) = self.get(target) else {
            return Err(error::unknown_process());
        };
        let trapping = process.is_trapping_exit();
        let effect = match exit_policy(signal.reason, trapping, &self.atoms) {
            ExitPolicy::Ignore => ExitEffect::Ignored,
            ExitPolicy::Trap => ExitEffect::Trapped,
            ExitPolicy::Terminate(_) => ExitEffect::Terminated,
        };

        // Generate: enqueue, never apply inline. The signaller's off-heap handles are
        // snapshotted because the target may be the currently executing process, which cannot
        // be moved out of the table to copy against a borrowed source.
        let fallback = OffHeapTable::new();
        let src = self
            .source_off_heap(target, signal.from, &fallback)
            .map(snapshot_off_heap);
        self.enqueue_exit_signal(target, signal.from, signal.reason, src.as_ref())?;

        // Eagerly service unless the target is the currently executing process, whose signal
        // is deferred to its next schedule-in.
        let mut pending = VecDeque::new();
        self.schedule_service(target, &mut pending);
        // No port table here: a cascade that terminates a process drains its outgoing port
        // monitors but cannot remove the incoming entries (a caller holding the table should
        // terminate through `terminate_with_ports`/`service_signals_with_ports`).
        let mut ports: Option<&mut PortTable> = None;
        self.drain_signals(&mut ports, &mut pending)?;
        Ok(effect)
    }

    /// Links `a` and `b` (both directions).
    ///
    /// Links are **set** semantics, not reference-counted: a re-link of an already-linked pair
    /// changes nothing (see [`crate::link::Links`]).
    ///
    /// A **non-live `b`** (a dead pid, or one that never existed) is not an error and records no
    /// link: the oracle answers `true` and signals a `noproc` exit **from that pid** at the
    /// linking caller, so a **trapping** caller is handed `{'EXIT', b, noproc}` and a
    /// non-trapping one is terminated with `noproc`. The signal goes through
    /// [`ProcessTable::apply_exit_signal`] (no new mechanism), so it inherits that path's
    /// eager-vs-deferred rule: a caller that is *not* currently executing sees the effect before
    /// this returns, while a `Running` caller — the one inside `erlang:link/1` — has it queued
    /// and applied at its next schedule-in, exactly as `erlang:exit(self(), Reason)` already is.
    ///
    /// Documented approximation: the oracle *raises* `error:noproc` in the non-trapping caller
    /// (whose uncaught reason is `{noproc, Stacktrace}`), whereas this signals the exit reason as
    /// the bare atom `noproc`. The trapping case matches the oracle exactly; the raise's
    /// stacktrace shape belongs to `beam_rt`'s link floor.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the **caller** `a` is unknown; propagates the
    /// signal path's allocation/termination errors for a non-live target.
    pub fn link(&mut self, a: ProcessId, b: ProcessId) -> Result<()> {
        if !self.contains(a) {
            return Err(error::unknown_process());
        }
        if !self.contains(b) {
            // Nothing to link to: the oracle signals `noproc` from the target at the caller (a
            // pid the process table does not hold is exactly the "there is no such process" the
            // signal reports). `b == a` cannot reach this branch — `a` is live here.
            let reason = self.atoms.noproc_term();
            return self
                .apply_exit_signal(a, ExitSignal { from: b, reason })
                .map(|_effect| ());
        }
        if a == b {
            // A self-link would only ever signal the process to itself on exit, which
            // termination already handles; keep the link set free of it.
            return Ok(());
        }
        if let Some(process) = self.get_mut(a) {
            process.links_mut().add(b);
        }
        if let Some(process) = self.get_mut(b) {
            process.links_mut().add(a);
        }
        Ok(())
    }

    /// Unlinks `a` and `b` (both directions).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when either pid is unknown.
    pub fn unlink(&mut self, a: ProcessId, b: ProcessId) -> Result<()> {
        if !self.contains(a) || !self.contains(b) {
            return Err(error::unknown_process());
        }
        if a == b {
            return Ok(());
        }
        if let Some(process) = self.get_mut(a) {
            process.links_mut().remove(b);
        }
        if let Some(process) = self.get_mut(b) {
            process.links_mut().remove(a);
        }
        Ok(())
    }

    /// Sends an exit signal from `from` to `to` with `reason` (`erlang:exit/2`).
    ///
    /// # Errors
    ///
    /// Propagates [`ProcessTable::apply_exit_signal`]'s errors.
    pub fn send_exit(&mut self, from: ProcessId, to: ProcessId, reason: Eterm) -> Result<()> {
        self.apply_exit_signal(to, ExitSignal { from, reason })?;
        Ok(())
    }

    /// Installs a monitor from `watcher` on `target` and returns its reference.
    ///
    /// A **live** `target` records both halves of the monitor (an outgoing [`Monitor`] on the
    /// watcher, an incoming [`MonitoredBy`] on the target). A `target` that is not a live local
    /// process — a dead pid, or a pid that never existed — is **not** an error: the reference is
    /// still allocated and the watcher immediately receives the
    /// `{'DOWN', Ref, process, Target, noproc}` the oracle delivers, and **no** monitor entry is
    /// recorded (there is nothing left to watch).
    ///
    /// This is the `erlang:monitor/2` form (no `{tag, Tag}` option); it delegates to
    /// [`ProcessTable::monitor_with_tag`] with `tag = None`, so the notification carries the
    /// default `'DOWN'` head atom.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the **watcher** is unknown; propagates the message
    /// builder's `BeamError::Alloc` when the immediate DOWN does not fit the watcher's heap.
    pub fn monitor(
        &mut self,
        watcher: ProcessId,
        target: ProcessId,
        kind: MonitorKind,
    ) -> Result<MonitorRef> {
        self.monitor_with_tag(watcher, target, kind, None)
    }

    /// [`ProcessTable::monitor`] with an `erlang:monitor/3` `{tag, Tag}` **user tag**: the
    /// eventual `{'DOWN', Ref, Kind, Target, Reason}` becomes `{Tag, Ref, Kind, Target, Reason}`.
    ///
    /// `tag = None` reproduces [`ProcessTable::monitor`] byte-for-byte. A tag that is not a
    /// heap-independent immediate (see `monitor.rs`'s user-tag contract) is refused with the
    /// typed [`error::unsupported_tag`] rather than stored as a word a collection could
    /// invalidate.
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor`], plus [`error::unsupported_tag`] for a non-immediate tag.
    pub fn monitor_with_tag(
        &mut self,
        watcher: ProcessId,
        target: ProcessId,
        kind: MonitorKind,
        tag: Option<Eterm>,
    ) -> Result<MonitorRef> {
        if !self.contains(watcher) {
            return Err(error::unknown_process());
        }
        let tag = check_tag(tag)?;
        let reference = self.refs.next();
        if !self.contains(target) {
            // Nothing to watch: the monitor is born down (`noproc`), and the watcher is told so
            // right away. No entry is recorded, so a later `demonitor` finds no local half — it
            // reports `false`, exactly like a monitor that is already gone.
            self.deliver_noproc_down(watcher, reference, target.to_eterm(), tag)?;
            return Ok(reference);
        }
        if let Some(process) = self.get_mut(watcher) {
            process.monitors_mut().push(Monitor {
                reference,
                target,
                kind,
            });
        }
        if let Some(process) = self.get_mut(target) {
            process.monitored_by_mut().push(MonitoredBy {
                reference,
                watcher,
                kind,
            });
        }
        // The tag is kept in the side table, not in the records above (see the field docs).
        remember_tag(&mut self.tags, reference, tag);
        Ok(reference)
    }

    /// Enqueues the immediate `{'DOWN', Ref, process, Target, noproc}` of a monitor on a
    /// non-live target into `watcher`'s mailbox — built on the watcher's own heap — and wakes a
    /// blocked watcher.
    ///
    /// When `tag` is `Some`, the head atom is the caller's user tag instead of `'DOWN'`
    /// (the `erlang:monitor/3` form). Mirrors
    /// [`ProcessTable::enqueue_port_down_message`]: the build goes through
    /// [`build_signal_term`] (so a full heap is a collection and a retry, never fatal) and the
    /// wake is recorded on the wake list. `target` must be an immediate (a pid term or a
    /// `{Name, Node}` pair the closure materialises itself); the `noproc` reason is an atom
    /// immediate, so nothing relocates. An unknown watcher is a no-op (the caller has already
    /// checked it, so only a mid-call death can reach this).
    fn deliver_noproc_down(
        &mut self,
        watcher: ProcessId,
        reference: MonitorRef,
        target: Eterm,
        tag: Option<Eterm>,
    ) -> Result<()> {
        let woke = {
            let atoms = &self.atoms;
            let reason = atoms.noproc_term();
            let Some(recipient) = lookup_mut(&mut self.slots, watcher) else {
                return Ok(());
            };
            let (term, off_heap) = build_signal_term(recipient, |heap| {
                let reference_term = reference.to_eterm(heap)?;
                let term = build_down_message_term(
                    heap,
                    atoms,
                    tag,
                    reference_term,
                    atoms.process(),
                    target,
                    reason,
                )?;
                Ok((term, Vec::<MsoHandle>::new()))
            })?;
            recipient
                .mailbox_mut()
                .enqueue(Message::new(term, target).with_off_heap(off_heap));
            recipient.wake()
        };
        if woke {
            self.ready.push(watcher);
        }
        Ok(())
    }

    /// Enqueues the immediate `{'DOWN', Ref, process, {Name, Node}, noproc}` of a **named**
    /// monitor on an unregistered name into `watcher`'s mailbox, and wakes a blocked watcher.
    ///
    /// When `tag` is `Some`, the head atom is the caller's user tag instead of `'DOWN'`.
    /// The `{Name, Node}` target is materialised on the watcher's own heap *inside* the
    /// [`build_signal_term`] closure, so a collect-and-retry never leaves a stale word behind.
    fn deliver_named_noproc_down(
        &mut self,
        watcher: ProcessId,
        reference: MonitorRef,
        name: Atom,
        node: Atom,
        tag: Option<Eterm>,
    ) -> Result<()> {
        let woke = {
            let atoms = &self.atoms;
            let reason = atoms.noproc_term();
            let Some(recipient) = lookup_mut(&mut self.slots, watcher) else {
                return Ok(());
            };
            let (term, target, off_heap) = build_signal_term(recipient, |heap| {
                let reference_term = reference.to_eterm(heap)?;
                let target = heap.tuple(&[make_atom(name), make_atom(node)])?;
                let term = build_down_message_term(
                    heap,
                    atoms,
                    tag,
                    reference_term,
                    atoms.process(),
                    target,
                    reason,
                )?;
                Ok((term, target, Vec::<MsoHandle>::new()))
            })?;
            recipient
                .mailbox_mut()
                .enqueue(Message::new(term, target).with_off_heap(off_heap));
            recipient.wake()
        };
        if woke {
            self.ready.push(watcher);
        }
        Ok(())
    }

    // --- named monitors ------------------------------------------------------

    /// Installs the atom that names the **local node** (e.g. `nonode@nohost`).
    ///
    /// A named monitor spells its DOWN target as `{Name, <this atom>}`, so the runtime (which
    /// owns the node's identity) must install it. Until then a named monitor is a typed
    /// [`error::unsupported`], never a guessed target.
    pub fn set_local_node(&mut self, node: Atom) {
        self.local_node = Some(node);
    }

    /// The atom naming the local node, once [`ProcessTable::set_local_node`] installed it.
    pub fn local_node(&self) -> Option<Atom> {
        self.local_node
    }

    /// Installs a monitor from `watcher` on the **registered name** `name`, resolving the target
    /// on the node `node` (`erlang:monitor(process, Name)`).
    ///
    /// A **live** registered name resolves to its process and records the ordinary monitor halves
    /// (an outgoing [`Monitor`] on the watcher, an incoming [`MonitoredBy`] on the target); the
    /// `{Name, node}` descriptor is recorded beside them, so the eventual `{'DOWN', …}` carries
    /// the **name** rather than the pid — exactly what the oracle does. An **unregistered** name
    /// (or one whose process has died) yields the immediate
    /// `{'DOWN', Ref, process, {Name, node}, noproc}` and records nothing.
    ///
    /// A [`MonitorKind::Port`] is rejected with the typed [`error::named_port_monitor`]: this
    /// registry resolves names to processes, so a **named port** monitor
    /// (`erlang:monitor(port, Name)`) has no faithful resolution here (see
    /// [`ProcessTable::monitor_port_term`] for the port-term form).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the watcher is unknown; propagates the message
    /// builder's `BeamError::Alloc` when the immediate DOWN does not fit the watcher's heap.
    pub fn monitor_named(
        &mut self,
        watcher: ProcessId,
        name: Atom,
        node: Atom,
        kind: MonitorKind,
    ) -> Result<MonitorRef> {
        self.monitor_named_with_tag(watcher, name, node, kind, None)
    }

    /// [`ProcessTable::monitor_named`] with an `erlang:monitor/3` `{tag, Tag}` **user tag**.
    ///
    /// `tag = None` reproduces [`ProcessTable::monitor_named`] byte-for-byte; a non-immediate
    /// tag is the typed [`error::unsupported_tag`] (see `monitor.rs`'s user-tag contract).
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor_named`], plus [`error::unsupported_tag`] for a non-immediate
    /// tag.
    pub fn monitor_named_with_tag(
        &mut self,
        watcher: ProcessId,
        name: Atom,
        node: Atom,
        kind: MonitorKind,
        tag: Option<Eterm>,
    ) -> Result<MonitorRef> {
        if !self.contains(watcher) {
            return Err(error::unknown_process());
        }
        if kind == MonitorKind::Port {
            // `erlang:monitor(port, Name)`: this registry resolves names to **processes**, so a
            // named port monitor has no faithful resolution here. Recording the process the
            // name resolves to would be a wrong (process) monitor; the typed diagnostic is the
            // honest answer.
            return Err(error::named_port_monitor());
        }
        let tag = check_tag(tag)?;
        let reference = self.refs.next();
        // `whereis` already resolves "registered but its process is gone" to `None`.
        let Some(target) = self.whereis(name) else {
            self.deliver_named_noproc_down(watcher, reference, name, node, tag)?;
            return Ok(reference);
        };
        if let Some(process) = self.get_mut(watcher) {
            process.monitors_mut().push(Monitor {
                reference,
                target,
                kind,
            });
        }
        if let Some(process) = self.get_mut(target) {
            process.monitored_by_mut().push(MonitoredBy {
                reference,
                watcher,
                kind,
            });
        }
        self.named_monitors
            .insert(reference, NamedTarget { name, node });
        // The tag is kept in the side table, not in the records above (see the field docs).
        remember_tag(&mut self.tags, reference, tag);
        Ok(reference)
    }

    // --- port monitors -------------------------------------------------------

    /// Installs a **port** monitor from `watcher` on `port` and returns its reference
    /// (`erlang:monitor(port, Port)`).
    ///
    /// Both halves are recorded: the watcher keeps the outgoing [`PortWatch`] — so
    /// [`ProcessTable::demonitor_port`] and `erlang:process_info(P, monitors)` can see it —
    /// and the port keeps the incoming [`PortMonitor`](crate::port::PortMonitor), so
    /// [`ProcessTable::port_closed`] resolves the pair with
    /// `{'DOWN', Ref, port, Port, Reason}`.
    ///
    /// A `port` that is **not** currently allocated is *not* an error: like
    /// [`ProcessTable::monitor`] on a dead pid, the monitor is born down and the watcher
    /// immediately receives `{'DOWN', Ref, port, Port, noproc}`, while no entry is recorded on
    /// either side (so a later [`ProcessTable::demonitor_port`] reports `false`). This is the
    /// oracle's behaviour (`erlang:monitor(port, ClosedPort)` answers a reference and fires a
    /// `noproc` DOWN).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the **watcher** is unknown; propagates the
    /// message builder's `BeamError::Alloc` when the immediate DOWN does not fit the watcher's
    /// heap.
    pub fn monitor_port(
        &mut self,
        watcher: ProcessId,
        port: PortId,
        ports: &mut PortTable,
    ) -> Result<MonitorRef> {
        self.monitor_port_with_tag(watcher, port, ports, None)
    }

    /// [`ProcessTable::monitor_port`] with an `erlang:monitor/3` `{tag, Tag}` **user tag**: the
    /// eventual `{'DOWN', Ref, port, Port, Reason}` becomes `{Tag, Ref, port, Port, Reason}`.
    ///
    /// `tag = None` reproduces [`ProcessTable::monitor_port`] byte-for-byte; a non-immediate tag
    /// is the typed [`error::unsupported_tag`] (see `monitor.rs`'s user-tag contract).
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor_port`], plus [`error::unsupported_tag`] for a non-immediate
    /// tag.
    pub fn monitor_port_with_tag(
        &mut self,
        watcher: ProcessId,
        port: PortId,
        ports: &mut PortTable,
        tag: Option<Eterm>,
    ) -> Result<MonitorRef> {
        if !self.contains(watcher) {
            return Err(error::unknown_process());
        }
        let tag = check_tag(tag)?;
        let reference = self.refs.next();
        if !ports.contains(port) {
            // Nothing to watch: the port is already gone. Tell the watcher right away and
            // record nothing, exactly as the pid path does for a dead pid.
            let reason = self.atoms.noproc_term();
            self.enqueue_port_down_message(watcher, reference, port, tag, reason)?;
            return Ok(reference);
        }
        ports.monitor(port, watcher, reference)?;
        if let Some(process) = self.get_mut(watcher) {
            process
                .port_monitors_mut()
                .push(PortWatch { reference, port });
        }
        // The tag is kept in the side table, not in the watch above (see the field docs).
        remember_tag(&mut self.tags, reference, tag);
        Ok(reference)
    }

    /// `erlang:monitor(port, Target)` against a term: the typed port-monitor entry point.
    ///
    /// `Target` must be a live-or-closed **port** term; a registered **name** is rejected with
    /// the typed [`error::named_port_monitor`] (the local registry holds processes only), so
    /// this never records a process monitor for a port-typed request.
    ///
    /// This is the `erlang:monitor/2` form (no `{tag, Tag}` option).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `target` is not a port — the oracle's `badarg` for
    /// `erlang:monitor(port, NotAPort)`; otherwise propagates [`ProcessTable::monitor_port`].
    pub fn monitor_port_term(
        &mut self,
        watcher: ProcessId,
        target: Eterm,
        ports: &mut PortTable,
    ) -> Result<MonitorRef> {
        self.monitor_port_term_with_tag(watcher, target, ports, None)
    }

    /// [`ProcessTable::monitor_port_term`] with an `erlang:monitor/3` `{tag, Tag}` **user tag**.
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor_port_term`], plus [`error::unsupported_tag`] for a
    /// non-immediate tag.
    pub fn monitor_port_term_with_tag(
        &mut self,
        watcher: ProcessId,
        target: Eterm,
        ports: &mut PortTable,
        tag: Option<Eterm>,
    ) -> Result<MonitorRef> {
        if is_atom(target) {
            return Err(error::named_port_monitor());
        }
        let port = PortId::from_eterm(target)?;
        self.monitor_port_with_tag(watcher, port, ports, tag)
    }

    /// Removes the **port** monitor `watcher` holds under `reference`, reporting whether it
    /// existed.
    ///
    /// This is the port counterpart of [`ProcessTable::demonitor`], which has no handle on a
    /// [`PortTable`]. The `erlang:demonitor/1` BIF layer must therefore try this **first** and
    /// fall back to [`ProcessTable::demonitor`] (then to
    /// [`ProcessTable::demonitor_flush`] for the `[flush]` option) when it reports `false`:
    ///
    /// ```text
    /// if !table.demonitor_port(watcher, reference, ports)? {
    ///     table.demonitor(watcher, reference)?;
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the **watcher** is unknown. A port that has
    /// already closed is **not** an error: [`ProcessTable::port_closed`] has already resolved
    /// and dropped the monitor, so this reports `false` (like `demonitor` of a monitor that is
    /// already gone).
    pub fn demonitor_port(
        &mut self,
        watcher: ProcessId,
        reference: MonitorRef,
        ports: &mut PortTable,
    ) -> Result<bool> {
        if !self.contains(watcher) {
            return Err(error::unknown_process());
        }
        let watch = self.get_mut(watcher).and_then(|process| {
            let entries = process.port_monitors_mut();
            let index = entries
                .iter()
                .position(|watch| watch.reference == reference)?;
            Some(entries.remove(index))
        });
        let Some(watch) = watch else {
            return Ok(false);
        };
        // The incoming half lives on the port; a port that closed earlier has already drained
        // it, so an unallocated id here is expected rather than an error.
        if ports.contains(watch.port) {
            ports.demonitor(watch.port, reference)?;
        }
        // The monitor is gone, so its user tag goes with it.
        forget_tag(&mut self.tags, reference);
        Ok(true)
    }

    /// Removes the outgoing monitor `watcher` holds under `reference`, reporting whether it
    /// existed.
    ///
    /// The reference may name a **local** monitor (the ordinary case, whose incoming entry on
    /// the target is dropped too) or an outgoing **remote** monitor, which is removed from the
    /// table and routed to the remote node as a `DOP_DEMONITOR_P` control signal.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the watcher is unknown; propagates the router's
    /// transport errors for a remote monitor.
    pub fn demonitor(&mut self, watcher: ProcessId, reference: MonitorRef) -> Result<bool> {
        if !self.contains(watcher) {
            return Err(error::unknown_process());
        }
        let local = self
            .get_mut(watcher)
            .and_then(|process| process.monitors_mut().remove(reference));
        let Some(monitor) = local else {
            // No local monitor under that reference: it may be an outgoing remote one.
            return self.demonitor_remote(watcher, reference);
        };
        // Drop the matching incoming entry from the (still living) target.
        if let Some(target) = self.get_mut(monitor.target) {
            target
                .monitored_by_mut()
                .retain(|entry| entry.reference != reference);
        }
        // A named monitor's `{Name, node}` descriptor dies with it — and so does its user tag.
        self.named_monitors.remove(&reference);
        forget_tag(&mut self.tags, reference);
        Ok(true)
    }

    /// `erlang:demonitor(Ref, [flush])`: [`ProcessTable::demonitor`] **and** the removal of any
    /// already-queued `{'DOWN', Ref, …}` from `watcher`'s mailbox.
    ///
    /// The monitor halves are dropped first (so the usual local/remote routing still happens),
    /// then the mailbox is scanned with the queue cursor API and every message recognised by
    /// [`payload_is_down_for`] as *this* reference's DOWN is removed. A fresh cursor is derived
    /// after each removal because a removal shifts every later message down by one.
    ///
    /// # Return value
    ///
    /// `true` when the monitor existed **or** a queued DOWN was removed; the BIF layer renders
    /// `erlang:demonitor/2` as unconditional `true` (the oracle's result), so this `bool` means
    /// "something was removed here", exactly like [`ProcessTable::demonitor`]'s.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `watcher` is unknown; propagates the router's
    /// transport errors for a remote monitor.
    pub fn demonitor_flush(
        &mut self,
        watcher: ProcessId,
        reference: MonitorRef,
    ) -> Result<bool> {
        if !self.contains(watcher) {
            return Err(error::unknown_process());
        }
        let existed = self.demonitor(watcher, reference)?;
        let down = self.atoms.down();
        let removed = {
            let Some(process) = self.get_mut(watcher) else {
                return Ok(existed);
            };
            let mailbox = process.mailbox_mut();
            let mut removed = false;
            let mut cursor = mailbox.cursor();
            loop {
                let position = mailbox.position(cursor);
                match mailbox.message_at(cursor).map(|message| message.payload()) {
                    Some(payload) if payload_is_down_for(payload, reference, down) => {
                        mailbox.remove_at(cursor);
                        removed = true;
                        // Positions shifted down by one: re-derive a cursor at the same index.
                        cursor = mailbox.cursor_at(position);
                    }
                    Some(_) => mailbox.advance(&mut cursor),
                    None => break,
                }
            }
            removed
        };
        Ok(existed || removed)
    }

    /// Removes the outgoing **remote** monitor `watcher` holds under `reference`, reporting
    /// whether it existed, and routes the `DOP_DEMONITOR_P` control signal.
    ///
    /// # Errors
    ///
    /// Propagates the router's transport errors.
    fn demonitor_remote(&mut self, watcher: ProcessId, reference: MonitorRef) -> Result<bool> {
        let (monitor, emptied) = {
            let Some(monitors) = self.remote_monitors.get_mut(&watcher) else {
                return Ok(false);
            };
            let Some(index) = monitors
                .iter()
                .position(|monitor| monitor.reference == reference)
            else {
                return Ok(false);
            };
            let monitor = monitors.remove(index);
            (monitor, monitors.is_empty())
        };
        if emptied {
            self.remote_monitors.remove(&watcher);
        }
        self.route(DistSignal::Demonitor {
            watcher,
            target: monitor.remote,
            reference,
        })?;
        Ok(true)
    }

    /// Terminates `pid` with `reason`, propagating to its links and monitors and cascading
    /// through the pending-signal worklist.
    ///
    /// The dying process's links are sent **exit signals** and its monitors `{'DOWN', …}`
    /// **signals**, queued on each receiver ([`ProcessTable::service_signals`] applies them);
    /// a receiver that is not the currently executing process is serviced eagerly, so the
    /// observable end state matches a synchronous propagation.
    ///
    /// This form has **no** [`PortTable`] handle, so the dying process's outgoing **port
    /// monitors** are drained but the matching *incoming* entries cannot be removed from the
    /// port table. A caller that has the port table should use
    /// [`ProcessTable::terminate_with_ports`] so a port-watcher's death also cleans the port
    /// side; this method is the `None`-ports delegation of it (today's behaviour).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the pid is unknown; propagates the
    /// propagation's allocation and signalling errors.
    pub fn terminate(&mut self, pid: ProcessId, reason: Eterm) -> Result<()> {
        let mut ports: Option<&mut PortTable> = None;
        self.terminate_and_cascade(&mut ports, pid, reason)
    }

    /// [`ProcessTable::terminate`] with the [`PortTable`] in hand, so a dying process's outgoing
    /// **port monitors** (`erlang:monitor(port, Port)`) also remove their matching *incoming*
    /// entries from `ports` — the outgoing [`PortWatch`] set is drained on death, and without
    /// this the port would keep a dangling monitor for a watcher that no longer exists
    /// (`erlang:port_info(Port, monitors)` would still list the dead pid).
    ///
    /// A link cascade reached from here (a peer that also dies) is cleaned too: the port table
    /// is threaded through the private cascade, exactly as the schedule-in path is through
    /// [`ProcessTable::service_signals_with_ports`].
    ///
    /// `ProcessTable` and `PortTable` are distinct types passed in separately, so `&mut self`
    /// and `&mut ports` are disjoint borrows (the same shape [`ProcessTable::port_closed`] uses).
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::terminate`].
    pub fn terminate_with_ports(
        &mut self,
        ports: &mut PortTable,
        pid: ProcessId,
        reason: Eterm,
    ) -> Result<()> {
        let mut ports: Option<&mut PortTable> = Some(ports);
        self.terminate_and_cascade(&mut ports, pid, reason)
    }

    /// The shared body of [`ProcessTable::terminate`] and
    /// [`ProcessTable::terminate_with_ports`]: verifies `pid` is live, runs the signal-generation
    /// half ([`ProcessTable::terminate_into`]) and then drains the cascade.
    fn terminate_and_cascade(
        &mut self,
        ports: &mut Option<&mut PortTable>,
        pid: ProcessId,
        reason: Eterm,
    ) -> Result<()> {
        if !self.contains(pid) {
            return Err(error::unknown_process());
        }
        let mut pending = VecDeque::new();
        self.terminate_into(ports, pid, reason, &mut pending)?;
        self.drain_signals(ports, &mut pending)
    }

    /// The signal-generation half of termination: reclaims `pid` and **queues** the signals its
    /// death implies on the affected receivers, pushing each receiver onto the `pending`
    /// worklist. It never recurses, so a cyclic link cascade is bounded.
    ///
    /// `reason` may live in `pid`'s own heap (a terminated exit reason queued for it); it is
    /// copied into every receiver's heap before `pid`'s heap is released.
    ///
    /// `ports` is `Some` when the caller holds the [`PortTable`]: the dying process's drained
    /// outgoing [`PortWatch`]es then remove their incoming [`PortMonitor`](crate::port::PortMonitor)
    /// entries. The **process-death observer** (see [`ProcessExitObserver`]) is notified exactly
    /// once, here, after the death has been propagated and before `drop(process)`.
    fn terminate_into(
        &mut self,
        ports: &mut Option<&mut PortTable>,
        pid: ProcessId,
        reason: Eterm,
        pending: &mut VecDeque<ProcessId>,
    ) -> Result<()> {
        // A process can be signalled twice within one propagation (two links into the same
        // target); the second visit finds the slot already reclaimed and is a no-op.
        let Some(mut process) = self.take(pid) else {
            return Ok(());
        };

        // The registered name, captured before the registry entry is dropped below and before
        // the dying heap is released: the exit observer needs it (see `ProcessExit::name`).
        let name = self
            .names
            .iter()
            .find(|(_, registered)| **registered == pid)
            .map(|(name, _)| *name);

        // A dying process keeps no name registration.
        self.names.retain(|_, registered| *registered != pid);
        // Snapshot every relationship before the PCB is recycled.
        let links = process.links_mut().drain();
        let watchers: Vec<MonitoredBy> = core::mem::take(process.monitored_by_mut());
        let outgoing = process.monitors_mut().drain();
        // The outgoing **port** monitors are drained here too (they cannot outlive the watcher):
        // their matching incoming entries are removed from the port table below, and the exit
        // observer is shown which ports the dying process was watching. `PortWatch` is a pair of
        // plain values, so this snapshot is heap-independent.
        let port_monitors = core::mem::take(process.port_monitors_mut());
        process.set_exit_reason(reason);
        process.set_state(ProcessState::Terminated);
        // `process` stays alive until the end of this body: `reason` lives in its heap, so
        // every receiver must be handed a *copy* before the dying heap is released (the drop
        // sits after the loops). The dying process's own pending signals go with it.
        //
        // Each receiver is *recorded* here and scheduled for servicing **after** the drop: the
        // dying process has already been reclaimed (`take` above), so `is_running` would report
        // it wrongly, and — more importantly — a receiver that is a still-`Running` process
        // must keep its signal pending. The eager-vs-deferred decision is therefore made by
        // `schedule_service`, once the receivers are fully populated.
        let mut receivers: Vec<ProcessId> = Vec::new();

        // 1. Links: drop the back-link, then queue an exit signal on the peer. The peer decides
        //    (through `exit_policy`, at service time) whether it terminates or traps.
        for target in links {
            if let Some(peer) = self.get_mut(target) {
                peer.links_mut().remove(pid);
            }
            if !self.contains(target) {
                // The peer died earlier in this same propagation.
                continue;
            }
            self.enqueue_exit_signal(target, pid, reason, Some(process.off_heap()))?;
            receivers.push(target);
        }

        // 2. Monitors: every watcher is told the target is down, carrying the monitor's
        //    `erlang:monitor/3` user tag when it has one. The records hold no tag field, so it is
        //    read from the side table; the DOWN that consumes it is its last use, so the entry is
        //    dropped as soon as the signal is queued.
        //
        //    The watcher's **outgoing** record goes too: the oracle resolves both halves when the
        //    target's death is signalled, so `erlang:process_info(W, monitors)` stops listing the
        //    monitor and a later `demonitor(Ref)` reports that it is gone (only
        //    `demonitor(Ref, [info])` distinguishes them — `demonitor/1` answers `true`
        //    unconditionally). Leaving it behind would dangle forever. The `named_monitors`
        //    `{Name, node}` descriptor is **not** dropped here: it is consumed by
        //    `build_and_enqueue_down` when the (possibly deferred) DOWN is materialised, and the
        //    DOWN must still carry the name.
        for watcher in watchers {
            if !self.contains(watcher.watcher) {
                // The watcher died earlier in this same propagation.
                continue;
            }
            let tag = self.tags.get(&watcher.reference).copied();
            self.enqueue_down_signal(
                watcher.watcher,
                watcher.reference,
                pid,
                watcher.kind,
                tag,
                reason,
                Some(process.off_heap()),
            )?;
            forget_tag(&mut self.tags, watcher.reference);
            if let Some(watcher_process) = self.get_mut(watcher.watcher) {
                watcher_process.monitors_mut().remove(watcher.reference);
            }
            receivers.push(watcher.watcher);
        }

        // 3. The dying process's own outgoing monitors vanish with it — and so do the
        //    `{Name, node}` descriptor and the user tag of any named/tagged monitor among them
        //    (nothing will ever report them down).
        for monitor in &outgoing {
            let reference = monitor.reference;
            if let Some(target) = self.get_mut(monitor.target) {
                target
                    .monitored_by_mut()
                    .retain(|entry| entry.reference != reference);
            }
            self.named_monitors.remove(&reference);
            forget_tag(&mut self.tags, reference);
        }

        // 4. Remote links and monitors: a dying process leaves the node's records, and every
        //    remote link partner must be told the process is gone. This runs before
        //    `drop(process)` because `reason` may live in the dying heap the router reads.
        for remote in self.remote_links.remove(&pid).unwrap_or_default() {
            self.route(DistSignal::Exit {
                from: pid,
                to: remote,
                reason,
            })?;
        }
        // Outgoing *remote* monitors die with the watcher: the remote node will simply
        // never see a `DOP_DEMONITOR_P`, which mirrors the local case above.
        self.remote_monitors.remove(&pid);

        // 5. Notify the process-death observer exactly once, now that the death has been
        //    propagated and *before* `drop(process)`: `reason` may live in the dying heap, so the
        //    payload is only valid up to the drop below. The observer is isolated (a panic is
        //    caught and logged, never propagated — see `ProcessExitObserver`).
        self.notify_process_exit(pid, reason, name, &port_monitors);

        // 6. Port monitors: the dying process was watching every `port_monitors` entry, so its
        //    matching *incoming* `PortMonitor` must be removed from the port table — otherwise a
        //    port would keep a dangling monitor for a watcher that no longer exists
        //    (`erlang:port_info(Port, monitors)` would still list the dead pid). With no port
        //    table in hand this is skipped (the observer still saw the watches). A port that has
        //    already closed has drained its side, so an unallocated id is not an error.
        if let Some(ports) = ports.as_deref_mut() {
            for watch in &port_monitors {
                if ports.contains(watch.port) {
                    ports.demonitor(watch.port, watch.reference)?;
                }
            }
        }

        // Every copy that read `reason` from the dying heap is done — release it.
        drop(process);

        // Now that the dying PCB is gone, schedule the receivers the death signalled: each is
        // serviced eagerly unless it is the currently executing process.
        for receiver in receivers {
            self.schedule_service(receiver, pending);
        }
        Ok(())
    }

    /// Applies every signal queued for `pid`, in FIFO order, cascading to any process those
    /// signals affect.
    ///
    /// This is the **schedule-in** entry point: the scheduler calls it on the dequeued pid
    /// before running it, so a process signalled while it was blocked sees the signal before
    /// its next reduction. A pid that is no longer live has nothing to apply, so it is a no-op.
    ///
    /// This form has **no** [`PortTable`] handle: a cascade that *terminates* a process drains
    /// its outgoing port monitors but cannot remove the matching incoming entries from the port
    /// table. A caller holding the port table should use
    /// [`ProcessTable::service_signals_with_ports`]; this method is its `None`-ports delegation
    /// (today's behaviour).
    ///
    /// # Errors
    ///
    /// Propagates the message builders' `BeamError::Alloc` (after a collection) and the
    /// cascading termination's errors.
    pub fn service_signals(&mut self, pid: ProcessId) -> Result<()> {
        let mut ports: Option<&mut PortTable> = None;
        self.service_signals_and_cascade(&mut ports, pid)
    }

    /// [`ProcessTable::service_signals`] with the [`PortTable`] in hand, so a schedule-in cascade
    /// that terminates a process also removes that process's incoming port monitors (see
    /// [`ProcessTable::terminate_with_ports`], whose port-cleaning this reuses).
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::service_signals`].
    pub fn service_signals_with_ports(
        &mut self,
        ports: &mut PortTable,
        pid: ProcessId,
    ) -> Result<()> {
        let mut ports: Option<&mut PortTable> = Some(ports);
        self.service_signals_and_cascade(&mut ports, pid)
    }

    /// The shared body of [`ProcessTable::service_signals`] and
    /// [`ProcessTable::service_signals_with_ports`].
    fn service_signals_and_cascade(
        &mut self,
        ports: &mut Option<&mut PortTable>,
        pid: ProcessId,
    ) -> Result<()> {
        if !self.contains(pid) {
            return Ok(());
        }
        let mut pending = VecDeque::new();
        pending.push_back(pid);
        self.drain_signals(ports, &mut pending)
    }

    /// Drains the signal queues of every pid in `pending`, and of any process the servicing
    /// itself signals, until no queue is left to drain.
    ///
    /// Iterative — the `pending` worklist is the only recursion the cascade has, so a link
    /// cycle (A↔B) terminates in a bounded number of steps.
    ///
    /// `ports` is threaded through so a cascade that terminates a process can clean its incoming
    /// port monitors (see [`ProcessTable::terminate_with_ports`]).
    fn drain_signals(
        &mut self,
        ports: &mut Option<&mut PortTable>,
        pending: &mut VecDeque<ProcessId>,
    ) -> Result<()> {
        while let Some(pid) = pending.pop_front() {
            while self.apply_head_signal(ports, pid, pending)? {}
        }
        Ok(())
    }

    /// Applies the signal at the head of `pid`'s queue, removing it, and returns whether the
    /// process **survived** (`false` when the queue is empty, the process is gone, or the
    /// signal terminated it).
    ///
    /// A terminating signal adds the affected receivers to `pending` (through
    /// [`ProcessTable::terminate_into`]) rather than recurring.
    fn apply_head_signal(
        &mut self,
        ports: &mut Option<&mut PortTable>,
        pid: ProcessId,
        pending: &mut VecDeque<ProcessId>,
    ) -> Result<bool> {
        let Some(signal) = self.head_signal(pid) else {
            return Ok(false);
        };
        match signal {
            PendingSignal::Exit { from, reason } => {
                let Some(process) = self.get(pid) else {
                    return Ok(false);
                };
                let trapping = process.is_trapping_exit();
                match exit_policy(reason, trapping, &self.atoms) {
                    ExitPolicy::Ignore => {
                        // A `normal` reason to a non-trapping process is discarded.
                        self.pop_head_signal(pid);
                        Ok(true)
                    }
                    ExitPolicy::Trap => {
                        self.deliver_trapped_exit(pid, from)?;
                        Ok(true)
                    }
                    ExitPolicy::Terminate(reason) => {
                        // `reason` still lives in `pid`'s heap, which `terminate_into`
                        // reclaims last, so it can be copied to the peers first.
                        self.terminate_into(ports, pid, reason, pending)?;
                        Ok(false)
                    }
                }
            }
            PendingSignal::Down {
                reference,
                target,
                kind,
                tag,
                ..
            } => {
                self.deliver_down_message(pid, reference, target, kind, tag)?;
                Ok(true)
            }
        }
    }

    /// The signal at the head of `pid`'s queue, if any (a copy of the small descriptor).
    fn head_signal(&self, pid: ProcessId) -> Option<PendingSignal> {
        lookup(&self.slots, pid)?.signals().peek().copied()
    }

    /// The reason of the signal at the head of `pid`'s queue, if any.
    fn head_signal_reason(&self, pid: ProcessId) -> Option<Eterm> {
        lookup(&self.slots, pid)?
            .signals()
            .peek()
            .map(PendingSignal::reason)
    }

    /// Removes the signal at the head of `pid`'s queue.
    fn pop_head_signal(&mut self, pid: ProcessId) {
        if let Some(process) = lookup_mut(&mut self.slots, pid) {
            process.signals_mut().pop();
        }
    }

    /// Whether `pid` names the currently executing process (`ProcessState::Running`).
    fn is_running(&self, pid: ProcessId) -> bool {
        matches!(
            lookup(&self.slots, pid).map(Process::state),
            Some(ProcessState::Running)
        )
    }

    /// Adds `pid` to the `pending` service worklist **unless** it is the currently executing
    /// process, whose signals are deferred to its next schedule-in.
    ///
    /// This is the single place the eager-vs-deferred rule is decided; every signal-generation
    /// path funnels its receivers through it. The rule is stated in the module docs: a signal
    /// generated for a process that is *not* running right now is serviced before the generating
    /// operation returns (ERTS's early signal handling), which is what makes the observable end
    /// state of `terminate`/`apply_exit_signal`/`send_exit` match a synchronous propagation for
    /// a non-running receiver, while a running receiver keeps its signal pending.
    fn schedule_service(&self, pid: ProcessId, pending: &mut VecDeque<ProcessId>) {
        if !self.is_running(pid) {
            pending.push_back(pid);
        }
    }

    /// Builds the head signal's message, retrying once after a collection.
    ///
    /// The queued signal is an in-heap sub-rootset, so a heap-exhaustion collection relocates
    /// its `reason` in place: `build` is handed a freshly read reason each attempt. On success
    /// the head signal is removed and a blocked `pid` is woken (and recorded on the wake list).
    fn deliver_head_signal<F>(&mut self, pid: ProcessId, mut build: F) -> Result<()>
    where
        F: FnMut(&mut Self, Eterm) -> Result<()>,
    {
        let mut attempt = 0;
        loop {
            let Some(reason) = self.head_signal_reason(pid) else {
                // The queue emptied (or the process is gone): nothing to deliver.
                return Ok(());
            };
            match build(self, reason) {
                Ok(()) => break,
                Err(BeamError::Alloc(_)) if attempt == 0 => {
                    attempt += 1;
                    let Some(recipient) = lookup_mut(&mut self.slots, pid) else {
                        return Ok(());
                    };
                    recipient.collect()?;
                }
                Err(error) => return Err(error),
            }
        }
        self.pop_head_signal(pid);
        let woke = lookup_mut(&mut self.slots, pid)
            .map(|process| process.wake())
            .unwrap_or(false);
        if woke {
            self.ready.push(pid);
        }
        Ok(())
    }

    /// Delivers the head (exit) signal of `pid` as a `{'EXIT', from, reason}` message.
    ///
    /// The reason is already owned by `pid`'s heap, so it is built without copying (the
    /// earlier `copy_reason_onto` at enqueue time did the ownership transfer).
    fn deliver_trapped_exit(&mut self, pid: ProcessId, from: ProcessId) -> Result<()> {
        self.deliver_head_signal(pid, |this, reason| {
            this.build_and_enqueue_exit(pid, from, reason)
        })
    }

    /// Delivers the head (monitor-DOWN) signal of `pid` as a `{'DOWN', …}` message — or
    /// `{Tag, …}` when the queued signal carried an `erlang:monitor/3` user tag.
    fn deliver_down_message(
        &mut self,
        pid: ProcessId,
        reference: MonitorRef,
        target: ProcessId,
        kind: MonitorKind,
        tag: Option<Eterm>,
    ) -> Result<()> {
        self.deliver_head_signal(pid, |this, reason| {
            this.build_and_enqueue_down(pid, reference, target, kind, tag, reason)
        })
    }

    /// Builds `{'EXIT', from, reason}` on `pid`'s heap — `reason` already resident in that
    /// heap — and enqueues it as a message.
    fn build_and_enqueue_exit(
        &mut self,
        pid: ProcessId,
        from: ProcessId,
        reason: Eterm,
    ) -> Result<()> {
        let atoms = &self.atoms;
        let Some(recipient) = lookup_mut(&mut self.slots, pid) else {
            return Ok(());
        };
        enqueue_exit_on(recipient, atoms, from, reason)
    }

    /// Builds `{'DOWN', Ref, Kind, Target, reason}` on `pid`'s heap — `reason` already resident
    /// in that heap — and enqueues it as a message. When `tag` is `Some`, the head atom is the
    /// caller's `erlang:monitor/3` user tag instead of `'DOWN'`.
    ///
    /// A **named** monitor's DOWN carries the `{Name, node}` pair it was created with rather than
    /// the pid the name resolved to (the oracle's behaviour); an ordinary monitor carries the
    /// pid. The named descriptor is consumed here: the DOWN is its last use.
    fn build_and_enqueue_down(
        &mut self,
        pid: ProcessId,
        reference: MonitorRef,
        target: ProcessId,
        kind: MonitorKind,
        tag: Option<Eterm>,
        reason: Eterm,
    ) -> Result<()> {
        let atoms = &self.atoms;
        let named = self.named_monitors.get(&reference).copied();
        let Some(recipient) = lookup_mut(&mut self.slots, pid) else {
            return Ok(());
        };
        let kind_atom = match kind {
            MonitorKind::Process => atoms.process(),
            MonitorKind::Port => atoms.port(),
        };
        let reference_term = reference.to_eterm(recipient.heap_mut())?;
        let target_term = match named {
            Some(nt) => recipient
                .heap_mut()
                .tuple(&[make_atom(nt.name), make_atom(nt.node)])?,
            None => target.to_eterm(),
        };
        let term = build_down_message_term(
            recipient.heap_mut(),
            atoms,
            tag,
            reference_term,
            kind_atom,
            target_term,
            reason,
        )?;
        recipient
            .mailbox_mut()
            .enqueue(Message::new(term, target_term));
        self.named_monitors.remove(&reference);
        Ok(())
    }

    /// Enqueues an exit signal on `to`'s signal queue, copying `reason` out of the heap
    /// described by `src` (the signaller's off-heap table, or `None` for a self-signal whose
    /// reason already lives in `to`'s heap) so the queued reason always outlives the signaller.
    ///
    /// An unknown `to` is skipped: the caller is propagating a signal through the table and
    /// the target may have died earlier in the same propagation.
    fn enqueue_exit_signal(
        &mut self,
        to: ProcessId,
        from: ProcessId,
        reason: Eterm,
        src: Option<&OffHeapTable>,
    ) -> Result<()> {
        let Some(recipient) = lookup_mut(&mut self.slots, to) else {
            return Ok(());
        };
        let reason = copy_reason_onto(recipient, reason, src)?;
        recipient
            .signals_mut()
            .push(PendingSignal::exit(from, reason));
        Ok(())
    }

    /// Enqueues a monitor-DOWN signal on `to`'s signal queue, copying `reason` out of the heap
    /// described by `src` (see [`ProcessTable::enqueue_exit_signal`]). `tag` is the monitor's
    /// `erlang:monitor/3` user tag — read from the side table by the caller, since the records
    /// carry none — and is a heap-independent immediate, carried verbatim.
    fn enqueue_down_signal(
        &mut self,
        to: ProcessId,
        reference: MonitorRef,
        target: ProcessId,
        kind: MonitorKind,
        tag: Option<Eterm>,
        reason: Eterm,
        src: Option<&OffHeapTable>,
    ) -> Result<()> {
        let Some(recipient) = lookup_mut(&mut self.slots, to) else {
            return Ok(());
        };
        let reason = copy_reason_onto(recipient, reason, src)?;
        recipient
            .signals_mut()
            .push(PendingSignal::down(reference, target, kind, tag, reason));
        Ok(())
    }

    /// The number of signals queued for `pid` (pending-signal introspection).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `pid` is unknown.
    pub fn pending_signals(&self, pid: ProcessId) -> Result<usize> {
        self.get(pid)
            .map(Process::pending_signals)
            .ok_or_else(error::unknown_process)
    }

    /// The signal queue of `pid`, for iteration over the pending signals.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `pid` is unknown.
    pub fn signals(&self, pid: ProcessId) -> Result<&crate::signal_queue::SignalQueue> {
        self.get(pid)
            .map(Process::signals)
            .ok_or_else(error::unknown_process)
    }

    // --- remote (distributed) links and monitors -----------------------------

    /// Installs the distribution seam the table routes remote signals through.
    ///
    /// Without a router the table is **record-only**: local relationships are tracked but no
    /// wire control signal is emitted (see [`ProcessTable::route`]).
    pub fn set_dist_router(&mut self, router: Box<dyn DistSignalRouter>) {
        self.router = Some(router);
    }

    /// Takes the distribution seam out of the table, returning it to the caller.
    pub fn take_dist_router(&mut self) -> Option<Box<dyn DistSignalRouter>> {
        self.router.take()
    }

    /// Whether a distribution seam is installed.
    pub fn has_dist_router(&self) -> bool {
        self.router.is_some()
    }

    /// Routes `signal` through the distribution seam, if one is installed.
    ///
    /// With no seam the local record stands and the signal is dropped: the relationship is
    /// still known to the table, there is simply nowhere to send it.
    ///
    /// # Errors
    ///
    /// Propagates the router's transport failure (an unknown/unconnected node, an I/O error).
    fn route(&mut self, signal: DistSignal) -> Result<()> {
        match &mut self.router {
            Some(router) => router.route(signal),
            None => Ok(()),
        }
    }

    // --- process-death observer ----------------------------------------------

    /// Installs the process-death observer seam the table calls once per death.
    ///
    /// The observer is invoked from `ProcessTable::terminate_into` after a dying process's
    /// links/monitors have been signalled and before its heap is released — the one place the
    /// runtime can release the resources it allocated on the pid's behalf (the ETS tables the
    /// pid owns are the motivating case). Without an observer termination is unchanged
    /// (see [`ProcessExitObserver`]).
    pub fn set_process_exit_observer(&mut self, observer: Box<dyn ProcessExitObserver>) {
        self.exit_observer = Some(observer);
    }

    /// Takes the process-death observer out of the table, returning it to the caller.
    pub fn take_process_exit_observer(&mut self) -> Option<Box<dyn ProcessExitObserver>> {
        self.exit_observer.take()
    }

    /// Whether a process-death observer is installed.
    pub fn has_process_exit_observer(&self) -> bool {
        self.exit_observer.is_some()
    }

    /// Notifies the installed process-death observer that `pid` died with `reason`, exactly once
    /// per death.
    ///
    /// Called by [`ProcessTable::terminate_into`] after the death has been propagated and before
    /// the dying heap is released (so `reason`, which may live there, is still valid). A
    /// **panicking** observer is contained — the panic is caught, reported through
    /// [`beam_common`]'s logging and swallowed — so it can never abort or corrupt termination
    /// (see [`ProcessExitObserver`]'s failure policy). With no observer installed this is a
    /// no-op.
    fn notify_process_exit(
        &mut self,
        pid: ProcessId,
        reason: Eterm,
        name: Option<Atom>,
        port_monitors: &[PortWatch],
    ) {
        let Some(observer) = self.exit_observer.as_mut() else {
            return;
        };
        let exit = ProcessExit::new(pid, reason, name, port_monitors);
        // SAFETY (unwind): the observer receives only an immutable payload and holds no
        // reference to the table, so a panic inside it cannot leave the table half-updated —
        // see `ProcessExitObserver`'s failure policy. The panic is reported and contained so
        // termination still completes.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observer.process_exited(&exit);
        }));
        if outcome.is_err() {
            beam_common::beam_error!(
                "beam_process: process-exit observer panicked for pid {:?}; termination continues",
                pid
            );
        }
    }
    /// `erlang:link/1` against an arbitrary pid term: a local pid links through
    /// [`ProcessTable::link`], an external pid is recorded as a remote link and routed as a
    /// `DOP_LINK` control signal.
    ///
    /// The local branch inherits both of [`ProcessTable::link`]'s properties for a **non-live**
    /// local target: no error, and a `noproc` exit signal from that pid applied to `a` (deferred
    /// when `a` is currently executing).
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when `b` is not a pid; for a local `b`,
    /// propagates [`ProcessTable::link`]'s `BeamError::TermNotFound` when the **caller** `a` is
    /// unknown; for a remote `b`, `BeamError::TermNotFound` when `a` is unknown; and the
    /// router's transport errors.
    pub fn link_term(&mut self, a: ProcessId, b: Eterm) -> Result<()> {
        match PidTarget::from_eterm(b)? {
            PidTarget::Local(pid) => self.link(a, pid),
            PidTarget::Remote(remote) => {
                if !self.contains(a) {
                    return Err(error::unknown_process());
                }
                let links = self.remote_links.entry(a).or_default();
                // Like `Links::add`: a re-link of an already-linked pair changes nothing.
                if !links.contains(&remote) {
                    links.push(remote);
                }
                self.route(DistSignal::Link { from: a, to: remote })
            }
        }
    }

    /// `erlang:unlink/1` against an arbitrary pid term: a local pid unlinks through
    /// [`ProcessTable::unlink`], an external pid drops the recorded remote link and routes a
    /// `DOP_UNLINK_ID` control signal.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when `b` is not a pid; for a local `b`,
    /// propagates [`ProcessTable::unlink`]'s `BeamError::TermNotFound`; and the router's
    /// transport errors.
    pub fn unlink_term(&mut self, a: ProcessId, b: Eterm) -> Result<()> {
        match PidTarget::from_eterm(b)? {
            PidTarget::Local(pid) => self.unlink(a, pid),
            PidTarget::Remote(remote) => {
                let emptied = match self.remote_links.get_mut(&a) {
                    Some(links) => {
                        if let Some(index) = links.iter().position(|entry| *entry == remote) {
                            links.remove(index);
                        }
                        links.is_empty()
                    }
                    None => false,
                };
                if emptied {
                    // An emptied link set carries no information.
                    self.remote_links.remove(&a);
                }
                self.route(DistSignal::Unlink { from: a, to: remote })
            }
        }
    }

    /// `erlang:monitor/2` against an arbitrary pid, port **or name** term: a local pid monitors
    /// through [`ProcessTable::monitor`], an external pid is recorded as an outgoing remote
    /// monitor and routed as a `DOP_MONITOR_P` control signal, and an **atom** is a registered
    /// name resolved through [`ProcessTable::monitor_named`] on the local node.
    ///
    /// A **port** target cannot be monitored here: a port monitor's incoming half lives in a
    /// [`PortTable`], which this entry point has no handle on. The faithful entry points are
    /// [`ProcessTable::monitor_port`]/[`ProcessTable::monitor_port_term`], so a port target with
    /// [`MonitorKind::Port`] is a typed [`error::unsupported`] rather than a wrong result (and,
    /// as the oracle requires, a port target with [`MonitorKind::Process`] is `badarg`).
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when `target` is not a pid, port or atom, and
    /// for a port target with [`MonitorKind::Process`]; the typed [`error::unsupported`] when
    /// `target` is an atom but no local node has been installed
    /// ([`ProcessTable::set_local_node`]), when the atom names a **port** monitor
    /// ([`error::named_port_monitor`]), or for a port target with [`MonitorKind::Port`]; for a
    /// local `target`, propagates [`ProcessTable::monitor`]'s `BeamError::TermNotFound`; for a
    /// remote `target`, `BeamError::TermNotFound` when `watcher` is unknown; and the router's
    /// transport errors.
    pub fn monitor_term(
        &mut self,
        watcher: ProcessId,
        target: Eterm,
        kind: MonitorKind,
    ) -> Result<MonitorRef> {
        self.monitor_term_with_tag(watcher, target, kind, None)
    }

    /// [`ProcessTable::monitor_term`] with an `erlang:monitor/3` `{tag, Tag}` **user tag**: a
    /// local pid dispatches to [`ProcessTable::monitor_with_tag`] and an atom name to
    /// [`ProcessTable::monitor_named_with_tag`].
    ///
    /// A **remote** (external-pid) target with a tag is the typed [`error::unsupported`]: the
    /// monitor signal on the wire (`DistSignal::Monitor` → `DOP_MONITOR_P`) has no tag field, so
    /// the tag could not be carried to the node that will eventually build the `{Tag, …}` DOWN —
    /// a typed diagnostic rather than a silently untagged notification.
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor_term`], plus [`error::unsupported_tag`] for a non-immediate
    /// tag and [`error::unsupported`] for a tag on a remote target.
    pub fn monitor_term_with_tag(
        &mut self,
        watcher: ProcessId,
        target: Eterm,
        kind: MonitorKind,
        tag: Option<Eterm>,
    ) -> Result<MonitorRef> {
        let tag = check_tag(tag)?;
        if PortId::is_port(target) {
            return match kind {
                MonitorKind::Port => Err(error::unsupported(
                    "monitor/2: port target requires the port table (use monitor_port)",
                )),
                // `erlang:monitor(process, Port)` is `badarg` in the oracle.
                MonitorKind::Process => Err(error::bad_pid()),
            };
        }
        if is_atom(target) {
            if kind == MonitorKind::Port {
                return Err(error::named_port_monitor());
            }
            // `monitor(process, Name)` names a locally-registered process; the `{Name, LocalNode}`
            // target needs the node's atom, which only the runtime can install. Guessing one would
            // be a wrong result, so its absence is a typed diagnostic.
            let Some(node) = self.local_node else {
                return Err(error::unsupported(
                    "monitor/2: name target requires the local node (set_local_node)",
                ));
            };
            return self.monitor_named_with_tag(watcher, atom_val(target), node, kind, tag);
        }
        match PidTarget::from_eterm(target)? {
            PidTarget::Local(pid) => self.monitor_with_tag(watcher, pid, kind, tag),
            PidTarget::Remote(remote) => {
                if tag.is_some() {
                    // The wire monitor signal has no tag field (see the method docs).
                    return Err(error::unsupported(
                        "monitor/3: tag on a remote target (DOP_MONITOR_P carries no tag)",
                    ));
                }
                if !self.contains(watcher) {
                    return Err(error::unknown_process());
                }
                let reference = self.refs.next();
                self.remote_monitors
                    .entry(watcher)
                    .or_default()
                    .push(RemoteMonitor {
                        reference,
                        remote,
                        kind,
                    });
                self.route(DistSignal::Monitor {
                    watcher,
                    target: remote,
                    reference,
                    kind,
                })?;
                Ok(reference)
            }
        }
    }

    /// `erlang:exit/2` against an arbitrary pid term: a local pid exits through
    /// [`ProcessTable::send_exit`], an external pid is routed as a `DOP_EXIT` control signal.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when `to` is not a pid; for a local `to`,
    /// propagates [`ProcessTable::send_exit`]'s errors; and the router's transport errors.
    pub fn send_exit_term(&mut self, from: ProcessId, to: Eterm, reason: Eterm) -> Result<()> {
        match PidTarget::from_eterm(to)? {
            PidTarget::Local(pid) => self.send_exit(from, pid, reason),
            PidTarget::Remote(remote) => self.route(DistSignal::Exit {
                from,
                to: remote,
                reason,
            }),
        }
    }

    /// The remote link partners recorded for `pid`.
    ///
    /// A pid that is no longer live resolves to nothing — the same rule
    /// [`ProcessTable::whereis`] applies to a name whose process is gone.
    pub fn remote_links(&self, pid: ProcessId) -> &[RemotePid] {
        if !self.contains(pid) {
            return &[];
        }
        self.remote_links
            .get(&pid)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The outgoing remote monitors recorded for `pid`.
    ///
    /// A pid that is no longer live resolves to nothing (see
    /// [`ProcessTable::remote_links`]).
    pub fn remote_monitors(&self, pid: ProcessId) -> &[RemoteMonitor] {
        if !self.contains(pid) {
            return &[];
        }
        self.remote_monitors
            .get(&pid)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// A whole remote **node** went away (its distribution connection dropped): every local
    /// relationship with that node is resolved the way the signals would have been, had the
    /// node been alive to send them, and then purged.
    ///
    /// This is a collect-then-apply sweep, matching the crate's discipline (the table is the
    /// only place that mutates two processes at once, so it never mutates while iterating):
    ///
    /// 1. every outgoing monitor on `node` queues `{'DOWN', Ref, process, RemotePid, Reason}`
    ///    on its watcher and wakes a blocked watcher;
    /// 2. every link to `node` follows [`exit_policy`] on the local peer: `Ignore` does
    ///    nothing, `Trap` queues `{'EXIT', RemotePid, Reason}`, anything else terminates the
    ///    peer (with the usual cascading propagation);
    /// 3. every record mentioning `node` is purged.
    ///
    /// `reason` is the node-down reason (an immediate such as `noconnection`) and is embedded
    /// verbatim — there is no source heap to copy it from, so the caller must pass a term that
    /// does not live in a heap that may be recycled underneath this call.
    ///
    /// # Errors
    ///
    /// Propagates the message builders' `BeamError::Alloc` (after a collection) and the
    /// cascading termination's errors.
    pub fn node_down(&mut self, node: Eterm, reason: Eterm) -> Result<()> {
        // 1. Monitors: snapshot the (watcher, monitor) pairs that watch `node`.
        let down: Vec<(ProcessId, RemoteMonitor)> = self
            .remote_monitors
            .iter()
            .flat_map(|(watcher, monitors)| {
                monitors
                    .iter()
                    .filter(|monitor| monitor.remote.node() == node)
                    .map(move |monitor| (*watcher, *monitor))
            })
            .collect();
        for (watcher, monitor) in down {
            self.enqueue_remote_down_message(watcher, monitor.reference, monitor.remote, reason)?;
        }

        // 2. Links: snapshot the (peer, remote) pairs linked to `node`.
        let linked: Vec<(ProcessId, RemotePid)> = self
            .remote_links
            .iter()
            .flat_map(|(peer, partners)| {
                partners
                    .iter()
                    .filter(|remote| remote.node() == node)
                    .map(move |remote| (*peer, *remote))
            })
            .collect();
        for (peer, remote) in linked {
            let Some(process) = self.get(peer) else {
                // The peer died earlier in this sweep.
                continue;
            };
            let trapping = process.is_trapping_exit();
            match exit_policy(reason, trapping, &self.atoms) {
                ExitPolicy::Ignore => {}
                ExitPolicy::Trap => self.enqueue_remote_exit_message(peer, remote, reason)?,
                ExitPolicy::Terminate(peer_reason) => self.terminate(peer, peer_reason)?,
            }
        }

        // 3. Purge every record mentioning `node`: the relationships are gone with it.
        for partners in self.remote_links.values_mut() {
            partners.retain(|remote| remote.node() != node);
        }
        self.remote_links.retain(|_, partners| !partners.is_empty());
        for monitors in self.remote_monitors.values_mut() {
            monitors.retain(|monitor| monitor.remote.node() != node);
        }
        self.remote_monitors
            .retain(|_, monitors| !monitors.is_empty());
        Ok(())
    }

    /// Enqueues `{'DOWN', Ref, process, RemotePid, Reason}` into `watcher`'s mailbox — built on
    /// the watcher's own heap — and wakes a blocked watcher.
    ///
    /// An unknown watcher is skipped (it died earlier in the same sweep).
    fn enqueue_remote_down_message(
        &mut self,
        watcher: ProcessId,
        reference: MonitorRef,
        remote: RemotePid,
        reason: Eterm,
    ) -> Result<()> {
        let woke = {
            let atoms = &self.atoms;
            let Some(recipient) = lookup_mut(&mut self.slots, watcher) else {
                return Ok(());
            };
            let (term, from, off_heap) = build_signal_term(recipient, |heap| {
                let reference_term = reference.to_eterm(heap)?;
                let from = remote.to_eterm(heap)?;
                // A remote monitor carries no user tag (see `monitor_term_with_tag`), so the
                // DOWN keeps the default `'DOWN'` head atom.
                let term = build_down_message_remote(
                    heap,
                    atoms,
                    None,
                    reference_term,
                    remote,
                    reason,
                )?;
                Ok((term, from, Vec::<MsoHandle>::new()))
            })?;
            recipient
                .mailbox_mut()
                .enqueue(Message::new(term, from).with_off_heap(off_heap));
            recipient.wake()
        };
        if woke {
            self.ready.push(watcher);
        }
        Ok(())
    }

    /// Enqueues `{'EXIT', RemotePid, Reason}` into `peer`'s mailbox — built on the peer's own
    /// heap — and wakes a blocked peer.
    ///
    /// An unknown peer is skipped (it died earlier in the same sweep).
    fn enqueue_remote_exit_message(
        &mut self,
        peer: ProcessId,
        remote: RemotePid,
        reason: Eterm,
    ) -> Result<()> {
        let woke = {
            let atoms = &self.atoms;
            let Some(recipient) = lookup_mut(&mut self.slots, peer) else {
                return Ok(());
            };
            let (term, from, off_heap) = build_signal_term(recipient, |heap| {
                let from = remote.to_eterm(heap)?;
                let term = build_exit_message_term(heap, atoms, from, reason)?;
                Ok((term, from, Vec::<MsoHandle>::new()))
            })?;
            recipient
                .mailbox_mut()
                .enqueue(Message::new(term, from).with_off_heap(off_heap));
            recipient.wake()
        };
        if woke {
            self.ready.push(peer);
        }
        Ok(())
    }

    // --- name registry -------------------------------------------------------

    /// Registers `pid` under `name` (`erlang:register/2`).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when the name is already taken **or** when `pid` is not a live
    /// local process — `erlang:register/2` raises `badarg` for a dead/foreign pid, and a name
    /// must never be bound to a process that can never run.
    pub fn register(&mut self, name: Atom, pid: ProcessId) -> Result<()> {
        if self.names.contains_key(&name) {
            return Err(error::name_taken());
        }
        if !self.contains(pid) {
            return Err(error::bad_pid());
        }
        self.names.insert(name, pid);
        Ok(())
    }

    /// Removes `name` from the registry (`erlang:unregister/1`).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when the name is not registered — `erlang:unregister/1`
    /// raises `badarg`, not `TermNotFound`.
    pub fn unregister(&mut self, name: Atom) -> Result<()> {
        match self.names.remove(&name) {
            Some(_) => Ok(()),
            None => Err(error::bad_pid()),
        }
    }

    /// The process registered under `name`, if any.
    pub fn whereis(&self, name: Atom) -> Option<ProcessId> {
        let pid = self.names.get(&name).copied()?;
        // A name whose process is gone resolves to nothing.
        if self.contains(pid) {
            Some(pid)
        } else {
            None
        }
    }

    /// Iterates the registered `(name, pid)` pairs.
    pub fn registered(&self) -> impl Iterator<Item = (Atom, ProcessId)> + '_ {
        self.names
            .iter()
            .filter(|(_, pid)| self.contains(**pid))
            .map(|(name, pid)| (*name, *pid))
    }

    /// The name `pid` is registered under (`erlang:process_info(Pid, registered_name)`).
    ///
    /// `None` for an unregistered process **and** for a pid that is not live: a stale
    /// registration left behind by a dead process reports nothing, the same rule
    /// [`ProcessTable::whereis`] applies in the other direction.
    pub fn name_of(&self, pid: ProcessId) -> Option<Atom> {
        if !self.contains(pid) {
            return None;
        }
        self.names
            .iter()
            .find(|(_, registered)| **registered == pid)
            .map(|(name, _)| *name)
    }

    // --- message builders ----------------------------------------------------

    /// The off-heap table a signal's reason must be copied *from*: the source process's table,
    /// `None` when the source and the recipient are the same process (the reason then already
    /// lives in the recipient's own heap), or `fallback` when the source is not (or no longer)
    /// a live process.
    fn source_off_heap<'a>(
        &'a self,
        recipient: ProcessId,
        source: ProcessId,
        fallback: &'a OffHeapTable,
    ) -> Option<&'a OffHeapTable> {
        if source == recipient {
            None
        } else {
            Some(self.get(source).map(Process::off_heap).unwrap_or(fallback))
        }
    }

    // --- port close propagation ----------------------------------------------

    /// Reports that `port` has closed with `reason`: delivers
    /// `{'DOWN', Ref, port, Port, Reason}` to every process monitoring the port (via
    /// [`PortTable::close`]) and an `{'EXIT', Port, Reason}` signal to every process linked
    /// to it.
    ///
    /// A monitoring watcher's outgoing [`PortWatch`] is removed here too — the port's death
    /// resolves the monitor on both sides, exactly as [`ProcessTable::terminate`] resolves a
    /// process monitor — so `erlang:process_info(Watcher, monitors)` stops listing it.
    ///
    /// A linked process follows [`exit_policy`]: a trapping peer receives the `{'EXIT', …}`
    /// message, a non-trapping one terminates with `reason` (cascading through its own links
    /// and monitors), and a `normal` reason is ignored. A peer that already died is skipped.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `port` is not allocated; propagates the message
    /// builders' `BeamError::Alloc` (after a collection) and the cascading termination's
    /// errors.
    pub fn port_closed(&mut self, ports: &mut PortTable, port: PortId, reason: Eterm) -> Result<()> {
        let closed = ports.close(port)?;

        // 1. Monitors: the port's death resolves the incoming half here (drained by `close`)
        //    and the watcher's outgoing half below; then the watcher is told the port is down.
        //    The records hold no tag field, so the monitor's `erlang:monitor/3` user tag is read
        //    from the side table; the DOWN built below is its last use, so the entry is dropped.
        for monitor in closed.monitors {
            if let Some(watcher) = self.get_mut(monitor.watcher) {
                watcher
                    .port_monitors_mut()
                    .retain(|watch| watch.reference != monitor.reference);
            }
            let tag = self.tags.get(&monitor.reference).copied();
            forget_tag(&mut self.tags, monitor.reference);
            self.enqueue_port_down_message(monitor.watcher, monitor.reference, port, tag, reason)?;
        }

        // 2. Links: each linked process either traps the exit or follows it.
        for pid in closed.links {
            let Some(peer) = self.get(pid) else {
                // The peer died earlier.
                continue;
            };
            let trapping = peer.is_trapping_exit();
            match exit_policy(reason, trapping, &self.atoms) {
                ExitPolicy::Ignore => {}
                ExitPolicy::Trap => self.enqueue_port_exit_message(pid, port, reason)?,
                ExitPolicy::Terminate(peer_reason) => self.terminate_with_ports(ports, pid, peer_reason)?,
            }
        }
        Ok(())
    }

    /// Enqueues `{'DOWN', Ref, port, Port, Reason}` into `to`'s mailbox — built on `to`'s own
    /// heap — and wakes `to` when it was blocked. With `tag = Some`, the head atom is the
    /// monitor's `erlang:monitor/3` user tag (`{Tag, Ref, port, Port, Reason}`).
    ///
    /// An unknown `to` is skipped (the watcher died earlier).
    fn enqueue_port_down_message(
        &mut self,
        to: ProcessId,
        reference: MonitorRef,
        port: PortId,
        tag: Option<Eterm>,
        reason: Eterm,
    ) -> Result<()> {
        let woke = {
            let atoms = &self.atoms;
            let Some(recipient) = lookup_mut(&mut self.slots, to) else {
                return Ok(());
            };
            let from = port.to_eterm();
            let (term, off_heap) = build_signal_term(recipient, |heap| {
                let reference_term = reference.to_eterm(heap)?;
                let term =
                    build_down_message_port(heap, atoms, tag, reference_term, port, reason)?;
                Ok((term, Vec::<MsoHandle>::new()))
            })?;
            recipient
                .mailbox_mut()
                .enqueue(Message::new(term, from).with_off_heap(off_heap));
            recipient.wake()
        };
        if woke {
            self.ready.push(to);
        }
        Ok(())
    }

    /// Enqueues `{'EXIT', Port, Reason}` into `to`'s mailbox — built on `to`'s own heap — and
    /// wakes `to` when it was blocked.
    ///
    /// An unknown `to` is skipped (the linked peer died earlier).
    fn enqueue_port_exit_message(&mut self, to: ProcessId, port: PortId, reason: Eterm) -> Result<()> {
        let woke = {
            let atoms = &self.atoms;
            let Some(recipient) = lookup_mut(&mut self.slots, to) else {
                return Ok(());
            };
            let from = port.to_eterm();
            let (term, off_heap) = build_signal_term(recipient, |heap| {
                let term = build_exit_message_term(heap, atoms, from, reason)?;
                Ok((term, Vec::<MsoHandle>::new()))
            })?;
            recipient
                .mailbox_mut()
                .enqueue(Message::new(term, from).with_off_heap(off_heap));
            recipient.wake()
        };
        if woke {
            self.ready.push(to);
        }
        Ok(())
    }
}#[cfg(test)]
mod tests {
    //! Unit tests for pid allocation/serial bumping/stale-pid rejection, the name
    //! registry, link/unlink, the exit-signal outcomes and monitor/demonitor. The cascading
    //! link/monitor propagation is covered by `tests/signals.rs` and the port
    //! ownership/link/monitor surface by `crate::port` and `tests/ports.rs`.

    use beam_term::prelude::{
        is_tuple, make_atom, make_small, tuple_element, tuple_size, AtomTable,
    };

    use super::*;

    /// A table with a fresh atom table behind it.
    fn table() -> ProcessTable {
        ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
    }

    /// A table plus the atom table its [`SignalAtoms`] were interned from, so a test can
    /// resolve the well-known names (`normal`, `kill`, …) back to terms.
    fn table_and_names() -> (ProcessTable, AtomTable) {
        let mut names = AtomTable::new();
        let atoms = SignalAtoms::from_table(&mut names);
        (ProcessTable::new(atoms), names)
    }

    /// The term for the interned name `name`.
    fn named(names: &AtomTable, name: &str) -> Eterm {
        make_atom(names.lookup(name).expect("the signal atoms interned it"))
    }

    #[test]
    fn alloc_pid_hands_out_distinct_pids() {
        let mut table = table();
        let first = table.alloc_pid().expect("room in the number space");
        let second = table.alloc_pid().expect("room in the number space");
        let third = table.alloc_pid().expect("room in the number space");
        assert_ne!(first, second);
        assert_ne!(second, third);
        assert_ne!(first, third);
        // Allocating a pid does not by itself install a process.
        assert_eq!(table.len(), 0);
        assert!(!table.contains(first));
    }

    #[test]
    fn spawn_installs_and_remove_frees_then_reuse_bumps_the_serial() {
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        assert!(table.contains(pid));
        assert_eq!(table.len(), 1);

        let process = table.remove(pid).expect("the pid was live");
        assert_eq!(process.pid(), pid);
        assert!(!table.contains(pid), "a removed pid no longer resolves");
        assert_eq!(table.len(), 0);

        let reused = table.spawn_default().expect("the freed number is reused");
        assert_eq!(reused.number(), pid.number());
        assert!(
            reused.serial() > pid.serial(),
            "a reused slot carries a bumped serial"
        );
        assert!(table.contains(reused));
        // The stale pid still does not resolve.
        assert!(!table.contains(pid));
        assert!(table.get(pid).is_none());
    }

    #[test]
    fn take_of_an_unknown_or_stale_pid_is_none() {
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        assert!(table.take(pid).is_some());
        assert!(table.take(pid).is_none(), "the second take finds nothing");
        assert!(table.take(ProcessId::new(999, 0)).is_none());
    }

    #[test]
    fn pids_iterates_exactly_the_live_processes() {
        let mut table = table();
        let a = table.spawn_default().expect("a");
        let b = table.spawn_default().expect("b");
        let c = table.spawn_default().expect("c");
        table.remove(b);

        let mut live: Vec<ProcessId> = table.pids().collect();
        live.sort();
        let mut expected = vec![a, c];
        expected.sort();
        assert_eq!(live, expected);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn get_mut_reaches_the_pcb() {
        let mut table = table();
        let pid = table.spawn_default().expect("a");
        table
            .get_mut(pid)
            .expect("the pcb is there")
            .bump_reductions(3);
        assert_eq!(table.get(pid).expect("still there").reductions(), 3);
        assert!(table.get(ProcessId::new(0, 7)).is_none());
    }

    #[test]
    fn deliver_wakes_a_blocked_target_and_records_it_on_the_wake_list() {
        let mut table = table();
        let sender = table.spawn_default().expect("sender");
        let receiver = table.spawn_default().expect("receiver");

        // A runnable target is enqueued but not woken.
        assert_eq!(
            table
                .deliver_simple(receiver, sender.to_eterm(), make_small(1))
                .expect("the receiver is live"),
            DeliverOutcome::Enqueued
        );
        assert!(table.woken().is_empty());

        // A blocked target is woken and put on the wake list.
        table
            .get_mut(receiver)
            .expect("the receiver is live")
            .set_state(ProcessState::Waiting);
        assert_eq!(
            table
                .deliver_simple(receiver, sender.to_eterm(), make_small(2))
                .expect("the receiver is live"),
            DeliverOutcome::Woke
        );
        assert_eq!(table.woken(), &[receiver]);
        assert_eq!(
            table.get(receiver).expect("the receiver is live").state(),
            ProcessState::Runnable
        );

        // Draining returns and clears the list.
        assert_eq!(table.drain_woken(), vec![receiver]);
        assert!(table.woken().is_empty());

        // Delivering to an unknown pid is an error.
        assert!(matches!(
            table.deliver_simple(ProcessId::new(999, 0), sender.to_eterm(), make_small(3)),
            Err(BeamError::TermNotFound)
        ));
    }

    #[test]
    fn taking_a_woken_process_clears_its_pending_wake() {
        let mut table = table();
        let sender = table.spawn_default().expect("sender");
        let receiver = table.spawn_default().expect("receiver");
        table
            .get_mut(receiver)
            .expect("the receiver is live")
            .set_state(ProcessState::Waiting);
        table
            .deliver_simple(receiver, sender.to_eterm(), make_small(1))
            .expect("the receiver is live");
        assert_eq!(table.woken(), &[receiver]);

        // The process dies before it is resumed: its stale wake must be dropped.
        table.remove(receiver).expect("the receiver is live");
        assert!(table.woken().is_empty());
    }

    #[test]
    fn a_trapped_exit_message_wakes_a_blocked_recipient() {
        let (mut table, _names) = table_and_names();
        let sender = table.spawn_default().expect("sender");
        let receiver = table.spawn_default().expect("receiver");
        table.get_mut(receiver).expect("receiver").set_trap_exit(true);
        table
            .get_mut(receiver)
            .expect("receiver")
            .set_state(ProcessState::Waiting);

        assert_eq!(
            table
                .apply_exit_signal(
                    receiver,
                    ExitSignal {
                        from: sender,
                        reason: make_small(9),
                    },
                )
                .expect("the receiver is live"),
            ExitEffect::Trapped
        );
        assert_eq!(table.get(receiver).expect("receiver").mailbox().len(), 1);
        assert_eq!(
            table.get(receiver).expect("receiver").state(),
            ProcessState::Runnable,
            "the trapped exit message woke the receiver"
        );
        assert_eq!(table.woken(), &[receiver]);
    }

    #[test]
    fn a_terminated_link_wakes_a_blocked_trapping_peer() {
        let (mut table, _names) = table_and_names();
        let a = table.spawn_default().expect("a");
        let b = table.spawn_default().expect("b");
        table.get_mut(b).expect("b").set_trap_exit(true);
        table.link(a, b).expect("both pids are live");
        table.get_mut(b).expect("b").set_state(ProcessState::Waiting);

        table.terminate(a, make_small(7)).expect("a is live");
        assert!(!table.contains(a));
        assert_eq!(
            table.get(b).expect("b").state(),
            ProcessState::Runnable,
            "the link exit message woke the trapping peer"
        );
        assert_eq!(table.woken(), &[b]);
    }

    #[test]
    fn a_down_message_wakes_a_blocked_watcher() {
        let (mut table, _names) = table_and_names();
        let watcher = table.spawn_default().expect("watcher");
        let target = table.spawn_default().expect("target");
        table
            .monitor(watcher, target, MonitorKind::Process)
            .expect("both pids are live");
        table
            .get_mut(watcher)
            .expect("watcher")
            .set_state(ProcessState::Waiting);

        table.terminate(target, make_small(3)).expect("target is live");
        assert_eq!(
            table.get(watcher).expect("watcher").state(),
            ProcessState::Runnable,
            "the down message woke the watcher"
        );
        assert_eq!(table.woken(), &[watcher]);
    }

    #[test]
    fn link_and_unlink_are_two_sided_and_reject_unknown_pids() {
        let mut table = table();
        let a = table.spawn_default().expect("a");
        let b = table.spawn_default().expect("b");

        table.link(a, b).expect("both live");
        assert!(table.get(a).expect("a").links().contains(b));
        assert!(table.get(b).expect("b").links().contains(a));
        // Re-linking is a no-op.
        table.link(a, b).expect("idempotent");
        assert_eq!(table.get(a).expect("a").links().len(), 1);

        table.unlink(a, b).expect("both live");
        assert!(!table.get(a).expect("a").links().contains(b));
        assert!(!table.get(b).expect("b").links().contains(a));

        // An unknown **caller** is still `TermNotFound` (the dead-target case is a signal, not an
        // error — see `link_to_a_non_live_target_signals_noproc_from_it`).
        assert!(matches!(
            table.link(ProcessId::new(999, 0), a),
            Err(BeamError::TermNotFound)
        ));
        assert!(matches!(
            table.link_term(ProcessId::new(999, 0), a.to_eterm()),
            Err(BeamError::TermNotFound)
        ));
        assert!(matches!(
            table.unlink(ProcessId::new(999, 0), a),
            Err(BeamError::TermNotFound)
        ));
    }

    #[test]
    fn link_to_a_non_live_target_signals_noproc_from_it() {
        // Oracle probe (OTP 29.1.1): a **trapping** caller's `link(DeadPid)` answers `true` and
        // delivers `{'EXIT', DeadPid, noproc}`; a non-trapping caller dies with the `noproc` raise
        // (whose uncaught reason is `{noproc, Stacktrace}`). Links are set semantics: nothing is
        // recorded for a pid that is not there.
        let (mut table, names) = table_and_names();
        let noproc = named(&names, "noproc");
        let dead = ProcessId::new(999, 0);

        // The caller of `erlang:link/1` is the **currently executing** process, so its signal is
        // queued (the `exit/2` model) rather than applied inline.
        let trapper = table.spawn_default().expect("trapper");
        table.get_mut(trapper).expect("trapper").set_trap_exit(true);
        table
            .get_mut(trapper)
            .expect("trapper")
            .set_state(ProcessState::Running);
        table.link(trapper, dead).expect("a non-live target is not an error");
        assert_eq!(
            table.pending_signals(trapper).expect("trapper is live"),
            1,
            "one noproc signal is queued for the caller"
        );
        assert!(
            table.get(trapper).expect("trapper").links().is_empty(),
            "no link is recorded to a pid that is not there"
        );
        assert!(table.get(dead).is_none(), "the target stays non-live");

        table.service_signals(trapper).expect("trapper is live");
        assert!(table.contains(trapper), "the trapping caller survives");
        let payload = table
            .get(trapper)
            .expect("trapper")
            .mailbox()
            .peek()
            .expect("the trapped exit is queued")
            .payload();
        // SAFETY: the payload is the live 3-tuple the signal builder wrote on the trapper's heap.
        unsafe {
            assert_eq!(tuple_size(payload), 3);
            assert_eq!(tuple_element(payload, 0), named(&names, "EXIT"));
            assert_eq!(
                tuple_element(payload, 1),
                dead.to_eterm(),
                "the signal came *from* the non-live target"
            );
            assert_eq!(tuple_element(payload, 2), noproc);
        }

        // A non-trapping caller is terminated with `noproc` once its signals are serviced.
        let victim = table.spawn_default().expect("victim");
        table
            .get_mut(victim)
            .expect("victim")
            .set_state(ProcessState::Running);
        table.link(victim, dead).expect("a non-live target is not an error");
        assert!(table.contains(victim));
        assert_eq!(table.pending_signals(victim).expect("victim"), 1);
        table.service_signals(victim).expect("victim is live");
        assert!(
            !table.contains(victim),
            "the non-trapping caller is terminated with noproc"
        );

        // `link_term`'s local branch routes through the same path.
        let via_term = table.spawn_default().expect("via_term");
        table.get_mut(via_term).expect("via_term").set_trap_exit(true);
        table
            .get_mut(via_term)
            .expect("via_term")
            .set_state(ProcessState::Running);
        table
            .link_term(via_term, dead.to_eterm())
            .expect("a non-live local target is not an error");
        assert_eq!(table.pending_signals(via_term).expect("via_term"), 1);
    }

    #[test]
    fn apply_exit_signal_ignores_traps_or_terminates() {
        let (mut table, names) = table_and_names();
        let normal = named(&names, "normal");

        // `normal` to a non-trapping target is ignored.
        let target = table.spawn_default().expect("target");
        let from = table.spawn_default().expect("from");
        assert_eq!(
            table
                .apply_exit_signal(
                    target,
                    ExitSignal { from, reason: normal }
                )
                .expect("a live target"),
            ExitEffect::Ignored
        );
        assert!(table.contains(target));

        // A non-atom "other" reason: to a non-trapping target it terminates.
        let other = make_small(1234);
        assert_eq!(
            table
                .apply_exit_signal(
                    target,
                    ExitSignal { from, reason: other }
                )
                .expect("a live target"),
            ExitEffect::Terminated
        );
        assert!(!table.contains(target));

        // A trapping target gets `{'EXIT', from, reason}` and survives.
        let target = table.spawn_default().expect("target");
        table.get_mut(target).expect("target").set_trap_exit(true);
        assert_eq!(
            table
                .apply_exit_signal(
                    target,
                    ExitSignal { from, reason: other }
                )
                .expect("a live target"),
            ExitEffect::Trapped
        );
        assert!(table.contains(target));
        let mailbox = table.get(target).expect("target").mailbox();
        assert_eq!(mailbox.len(), 1);
        // SAFETY: the payload is the live 3-tuple built by the signal builder.
        unsafe {
            let payload = mailbox.peek().expect("one message").payload();
            assert_eq!(tuple_size(payload), 3);
            assert_eq!(tuple_element(payload, 1), from.to_eterm());
            assert_eq!(tuple_element(payload, 2), other);
        }

        assert!(matches!(
            table.apply_exit_signal(
                ProcessId::new(999, 0),
                ExitSignal { from, reason: other }
            ),
            Err(BeamError::TermNotFound)
        ));
    }

    #[test]
    fn kill_is_untrappable() {
        let (mut table, names) = table_and_names();
        let kill = named(&names, "kill");

        let target = table.spawn_default().expect("target");
        let from = table.spawn_default().expect("from");
        table.get_mut(target).expect("target").set_trap_exit(true);
        assert_eq!(
            table
                .apply_exit_signal(target, ExitSignal { from, reason: kill })
                .expect("a live target"),
            ExitEffect::Terminated,
            "kill terminates even a trapping process"
        );
        assert!(!table.contains(target));
    }

    #[test]
    fn monitor_and_demonitor_keep_both_halves_in_sync() {
        let (mut table, names) = table_and_names();
        let watcher = table.spawn_default().expect("watcher");
        let target = table.spawn_default().expect("target");

        let reference = table
            .monitor(watcher, target, MonitorKind::Process)
            .expect("both live");
        assert_eq!(
            table
                .get(watcher)
                .expect("watcher")
                .monitors()
                .find(reference)
                .map(|m| m.target),
            Some(target)
        );
        assert_eq!(table.get(target).expect("target").monitored_by().len(), 1);

        assert!(table.demonitor(watcher, reference).expect("watcher live"));
        assert!(table.get(watcher).expect("watcher").monitors().is_empty());
        assert!(table.get(target).expect("target").monitored_by().is_empty());
        assert!(!table.demonitor(watcher, reference).expect("watcher live"));

        // An unknown *watcher* is still an error.
        assert!(matches!(
            table.demonitor(ProcessId::new(999, 0), reference),
            Err(BeamError::TermNotFound)
        ));

        // A pid that is not a live local process is NOT an error: the monitor is born down, its
        // `{'DOWN', Ref, process, Target, noproc}` is delivered immediately, and no half of the
        // monitor is recorded (there is nothing left to watch).
        let dead = ProcessId::new(999, 0);
        let down_atom = table.atoms.down();
        let mailbox_before = table.get(watcher).expect("watcher").mailbox().len();
        let dead_ref = table
            .monitor(watcher, dead, MonitorKind::Process)
            .expect("a non-live target is not an error");
        assert_ne!(dead_ref, reference);
        assert!(table.get(dead).is_none(), "the target stays non-live");
        let watcher_process = table.get(watcher).expect("watcher");
        assert_eq!(
            watcher_process.mailbox().len(),
            mailbox_before + 1,
            "the immediate noproc DOWN landed in the watcher's mailbox"
        );
        assert!(
            watcher_process.monitors().find(dead_ref).is_none(),
            "a non-live target records no outgoing monitor"
        );
        let payload = watcher_process
            .mailbox()
            .peek()
            .expect("the DOWN is queued")
            .payload();
        assert!(
            payload_is_down_for(payload, dead_ref, down_atom),
            "a 5-tuple `{{'DOWN', Ref, ..}}` for the fresh reference"
        );
        // SAFETY: the payload is the live 5-tuple the DOWN builder wrote.
        unsafe {
            assert_eq!(tuple_size(payload), 5);
            assert_eq!(tuple_element(payload, 0), named(&names, "DOWN"));
            assert_eq!(tuple_element(payload, 2), named(&names, "process"));
            assert_eq!(tuple_element(payload, 3), dead.to_eterm());
            assert_eq!(tuple_element(payload, 4), named(&names, "noproc"));
        }
    }

    #[test]
    fn demonitor_after_the_target_died_still_drops_the_outgoing_entry() {
        let mut table = table();
        let watcher = table.spawn_default().expect("watcher");
        let target = table.spawn_default().expect("target");
        let reference = table
            .monitor(watcher, target, MonitorKind::Process)
            .expect("both live");
        table.remove(target);
        assert!(table.demonitor(watcher, reference).expect("watcher live"));
        assert!(table.get(watcher).expect("watcher").monitors().is_empty());
    }

    #[test]
    fn demonitor_flush_removes_only_the_matching_down() {
        let mut table = table();
        let watcher = table.spawn_default().expect("watcher");

        // Two monitors on two non-live pids: each queues its own immediate noproc DOWN.
        let first = table
            .monitor(watcher, ProcessId::new(999, 0), MonitorKind::Process)
            .expect("a non-live target is not an error");
        let second = table
            .monitor(watcher, ProcessId::new(998, 0), MonitorKind::Process)
            .expect("a non-live target is not an error");
        assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 2);

        // A reference the table never handed out flushes nothing (and is not an error).
        let bogus = table.refs.next();
        assert!(!table.demonitor(watcher, bogus).expect("watcher live"));
        assert!(!table.demonitor_flush(watcher, bogus).expect("watcher live"));
        assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 2);

        // Flushing the second reference removes exactly its own DOWN.
        assert!(table
            .demonitor_flush(watcher, second)
            .expect("watcher live"));
        assert_eq!(
            table.get(watcher).expect("watcher").mailbox().len(),
            1,
            "only the second DOWN was flushed"
        );
        let remaining = table
            .get(watcher)
            .expect("watcher")
            .mailbox()
            .peek()
            .expect("one message")
            .payload();
        assert!(
            payload_is_down_for(remaining, first, table.atoms.down()),
            "the surviving DOWN is the first monitor's"
        );

        // The first DOWN goes too; re-flushing changes nothing.
        assert!(table.demonitor_flush(watcher, first).expect("watcher live"));
        assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 0);
        assert!(!table.demonitor_flush(watcher, first).expect("watcher live"));

        // An unknown watcher is still an error.
        assert!(matches!(
            table.demonitor_flush(ProcessId::new(999, 0), first),
            Err(BeamError::TermNotFound)
        ));
    }

    #[test]
    fn plain_demonitor_leaves_a_queued_down_while_flush_removes_it() {
        let mut table = table();
        let watcher = table.spawn_default().expect("watcher");
        let target = table.spawn_default().expect("target");

        let reference = table
            .monitor(watcher, target, MonitorKind::Process)
            .expect("both live");
        table.terminate(target, make_small(5)).expect("live");
        assert_eq!(
            table.get(watcher).expect("watcher").mailbox().len(),
            1,
            "the eagerly applied DOWN is queued"
        );

        // `demonitor/1` (no flush) drops the monitor half but leaves the DOWN queued.
        table.demonitor(watcher, reference).expect("watcher live");
        assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 1);

        // `demonitor/2` with `[flush]` removes it.
        assert!(table
            .demonitor_flush(watcher, reference)
            .expect("watcher live"));
        assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 0);
    }

    #[test]
    fn monitor_named_reports_the_name_node_target_on_death() {
        let (mut table, names) = table_and_names();
        let mut atoms_table = AtomTable::new();
        let name = atoms_table.intern("server");
        let node = atoms_table.intern("nonode@nohost");
        let watcher = table.spawn_default().expect("watcher");
        let target = table.spawn_default().expect("target");
        table.register(name, target).expect("the name is free");

        let reference = table
            .monitor_named(watcher, name, node, MonitorKind::Process)
            .expect("the name resolves to a live process");
        // The monitor halves are the ordinary ones: the name resolved to a pid.
        assert_eq!(
            table
                .get(watcher)
                .expect("watcher")
                .monitors()
                .find(reference)
                .map(|m| m.target),
            Some(target)
        );
        assert_eq!(table.get(target).expect("target").monitored_by().len(), 1);

        table.terminate(target, make_small(7)).expect("target live");
        let payload = table
            .get(watcher)
            .expect("watcher")
            .mailbox()
            .peek()
            .expect("the DOWN is queued")
            .payload();
        // SAFETY: the payload is the live 5-tuple the DOWN builder wrote.
        unsafe {
            assert_eq!(tuple_size(payload), 5);
            assert_eq!(tuple_element(payload, 0), named(&names, "DOWN"));
            assert_eq!(tuple_element(payload, 2), named(&names, "process"));
            let target_term = tuple_element(payload, 3);
            assert!(is_tuple(target_term), "the target is the `{{Name, Node}}` pair");
            assert_eq!(tuple_size(target_term), 2);
            assert_eq!(tuple_element(target_term, 0), make_atom(name));
            assert_eq!(tuple_element(target_term, 1), make_atom(node));
            assert_eq!(tuple_element(payload, 4), make_small(7), "the death reason");
        }
        // The descriptor is consumed by the DOWN it produced.
        assert!(table.named_monitors.is_empty());
    }

    #[test]
    fn monitor_named_of_an_unregistered_name_is_born_down() {
        let (mut table, names) = table_and_names();
        let mut atoms_table = AtomTable::new();
        let name = atoms_table.intern("nobody");
        let node = atoms_table.intern("nonode@nohost");
        let watcher = table.spawn_default().expect("watcher");
        let down_atom = table.atoms.down();

        assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 0);
        let reference = table
            .monitor_named(watcher, name, node, MonitorKind::Process)
            .expect("an unregistered name is not an error");
        let watcher_process = table.get(watcher).expect("watcher");
        assert_eq!(
            watcher_process.mailbox().len(),
            1,
            "the immediate noproc DOWN landed"
        );
        assert!(
            watcher_process.monitors().is_empty(),
            "an unregistered name records no monitor"
        );
        assert!(table.named_monitors.is_empty());
        let payload = watcher_process
            .mailbox()
            .peek()
            .expect("the DOWN is queued")
            .payload();
        assert!(payload_is_down_for(payload, reference, down_atom));
        // SAFETY: the payload is the live 5-tuple the DOWN builder wrote.
        unsafe {
            assert_eq!(tuple_size(payload), 5);
            assert_eq!(tuple_element(payload, 0), named(&names, "DOWN"));
            assert_eq!(tuple_element(payload, 2), named(&names, "process"));
            let target_term = tuple_element(payload, 3);
            assert_eq!(tuple_size(target_term), 2);
            assert_eq!(tuple_element(target_term, 0), make_atom(name));
            assert_eq!(tuple_element(target_term, 1), make_atom(node));
            assert_eq!(tuple_element(payload, 4), named(&names, "noproc"));
        }
    }

    #[test]
    fn monitor_term_routes_an_atom_target_to_the_named_path() {
        let (mut table, _names) = table_and_names();
        let mut atoms_table = AtomTable::new();
        let name = atoms_table.intern("server");
        let node = atoms_table.intern("nonode@nohost");
        let watcher = table.spawn_default().expect("watcher");
        let target = table.spawn_default().expect("target");
        table.register(name, target).expect("the name is free");

        // Without a local node installed, a name target is a typed unsupported — never a guessed
        // target, and never a recorded monitor.
        match table.monitor_term(watcher, make_atom(name), MonitorKind::Process) {
            Err(BeamError::Raise(raise)) => assert_eq!(
                raise.reason.as_atom(),
                Some("unsupported: monitor/2: name target requires the local node (set_local_node)")
            ),
            other => panic!("expected a typed unsupported raise, got {other:?}"),
        }
        assert!(table.get(watcher).expect("watcher").mailbox().is_empty());

        // With the node installed it routes to `monitor_named`.
        assert_eq!(table.local_node(), None);
        table.set_local_node(node);
        assert_eq!(table.local_node(), Some(node));
        let reference = table
            .monitor_term(watcher, make_atom(name), MonitorKind::Process)
            .expect("the name resolves");
        assert_eq!(
            table
                .get(watcher)
                .expect("watcher")
                .monitors()
                .find(reference)
                .map(|m| m.target),
            Some(target)
        );

        // A non-pid, non-atom term is still `badarg`.
        assert!(matches!(
            table.monitor_term(watcher, make_small(1), MonitorKind::Process),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn terminate_of_an_unknown_pid_is_term_not_found() {
        let mut table = table();
        assert!(matches!(
            table.terminate(ProcessId::new(1, 0), make_small(0)),
            Err(BeamError::TermNotFound)
        ));
    }

    #[test]
    fn terminate_drops_the_name_registration() {
        let mut table = table();
        let mut atoms_table = AtomTable::new();
        let name = atoms_table.intern("server");
        let pid = table.spawn_default().expect("server");
        table.register(name, pid).expect("the name is free");
        assert_eq!(table.whereis(name), Some(pid));

        table.terminate(pid, make_small(1)).expect("live");
        assert!(!table.contains(pid));
        assert_eq!(
            table.whereis(name),
            None,
            "a name whose process is gone is gone"
        );
    }

    #[test]
    fn name_registry_rejects_duplicates_and_unknown_unregister() {
        let mut table = table();
        let mut atoms_table = AtomTable::new();
        let name = atoms_table.intern("registered");
        let other = atoms_table.intern("unregistered");
        let pid = table.spawn_default().expect("a");
        let pid2 = table.spawn_default().expect("b");

        table.register(name, pid).expect("the name is free");
        assert!(matches!(table.register(name, pid2), Err(BeamError::BadArg)));
        // `erlang:register/2` of a pid that is not a live local process is `badarg`.
        assert!(matches!(
            table.register(other, ProcessId::new(999, 0)),
            Err(BeamError::BadArg)
        ));
        assert_eq!(table.whereis(name), Some(pid));
        assert_eq!(table.whereis(other), None);

        let registered: Vec<(Atom, ProcessId)> = table.registered().collect();
        assert_eq!(registered, vec![(name, pid)]);

        table.unregister(name).expect("it was registered");
        // `erlang:unregister/1` of an unregistered name is `badarg`, not `TermNotFound`.
        assert!(matches!(table.unregister(name), Err(BeamError::BadArg)));
        // A name registered to a dead process is not reported.
        table.register(name, pid2).expect("free again");
        table.remove(pid2);
        assert_eq!(table.registered().count(), 0);
    }

    #[test]
    fn register_rejects_a_dead_pid_and_name_of_reports_liveness() {
        let mut table = table();
        let mut atoms_table = AtomTable::new();
        let name = atoms_table.intern("server");
        let pid = table.spawn_default().expect("server");

        assert_eq!(table.name_of(pid), None, "not registered yet");
        table.register(name, pid).expect("the pid is live");
        assert_eq!(table.name_of(pid), Some(name));

        // A dead pid cannot be registered (nor keep a registration).
        let dead = {
            let dying = table.spawn_default().expect("dying");
            table.remove(dying);
            dying
        };
        assert!(matches!(
            table.register(atoms_table.intern("dead"), dead),
            Err(BeamError::BadArg)
        ));

        table.unregister(name).expect("it was registered");
        assert_eq!(table.name_of(pid), None, "an unregistered pid names nothing");

        // A stale registration left by a dead process reports nothing.
        table.register(name, pid).expect("free again");
        table.remove(pid);
        assert_eq!(
            table.name_of(pid),
            None,
            "a dead pid reports no name even if a registration lingers"
        );
    }

    #[test]
    fn is_process_alive_tracks_contains_and_exit_reason_reports_a_recorded_reason() {
        let mut table = table();
        let pid = table.spawn_default().expect("a");
        assert!(table.is_process_alive(pid));
        assert!(!table.is_process_alive(ProcessId::new(999, 0)));

        // A live process has no exit reason yet.
        assert_eq!(table.exit_reason(pid).expect("live"), None);
        table
            .get_mut(pid)
            .expect("live")
            .set_exit_reason(make_small(17));
        assert_eq!(
            table.exit_reason(pid).expect("live"),
            Some(make_small(17)),
            "the recorded reason is reported verbatim"
        );

        // Once the process is gone, the pid is unknown to the table.
        table.terminate(pid, make_small(17)).expect("live");
        assert!(!table.is_process_alive(pid));
        assert!(matches!(
            table.exit_reason(pid),
            Err(BeamError::TermNotFound)
        ));
        assert!(matches!(
            table.exit_reason(ProcessId::new(999, 0)),
            Err(BeamError::TermNotFound)
        ));
    }
}
