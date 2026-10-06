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
use beam_term::prelude::{Atom, Eterm};

use crate::error;
use crate::monitor::{Monitor, MonitorKind, MonitorRef, MonitoredBy, RefAllocator};
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
    /// The distribution seam (see [`DistSignalRouter`]); `None` means **record-only** — the
    /// local relationship is still tracked, only the wire signal is dropped (a node with no
    /// distribution configured).
    router: Option<Box<dyn DistSignalRouter>>,
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
            router: None,
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
        self.drain_signals(&mut pending)?;
        Ok(effect)
    }

    /// Links `a` and `b` (both directions).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when either pid is unknown.
    pub fn link(&mut self, a: ProcessId, b: ProcessId) -> Result<()> {
        if !self.contains(a) || !self.contains(b) {
            return Err(error::unknown_process());
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
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when either pid is unknown.
    pub fn monitor(
        &mut self,
        watcher: ProcessId,
        target: ProcessId,
        kind: MonitorKind,
    ) -> Result<MonitorRef> {
        if !self.contains(watcher) || !self.contains(target) {
            return Err(error::unknown_process());
        }
        let reference = self.refs.next();
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
        Ok(reference)
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
        Ok(true)
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
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the pid is unknown; propagates the
    /// propagation's allocation and signalling errors.
    pub fn terminate(&mut self, pid: ProcessId, reason: Eterm) -> Result<()> {
        if !self.contains(pid) {
            return Err(error::unknown_process());
        }
        let mut pending = VecDeque::new();
        self.terminate_into(pid, reason, &mut pending)?;
        self.drain_signals(&mut pending)
    }

    /// The signal-generation half of [`ProcessTable::terminate`]: reclaims `pid` and **queues**
    /// the signals its death implies on the affected receivers, pushing each receiver onto the
    /// `pending` worklist. It never recurses, so a cyclic link cascade is bounded.
    ///
    /// `reason` may live in `pid`'s own heap (a terminated exit reason queued for it); it is
    /// copied into every receiver's heap before `pid`'s heap is released.
    fn terminate_into(
        &mut self,
        pid: ProcessId,
        reason: Eterm,
        pending: &mut VecDeque<ProcessId>,
    ) -> Result<()> {
        // A process can be signalled twice within one propagation (two links into the same
        // target); the second visit finds the slot already reclaimed and is a no-op.
        let Some(mut process) = self.take(pid) else {
            return Ok(());
        };

        // A dying process keeps no name registration.
        self.names.retain(|_, registered| *registered != pid);

        // Snapshot every relationship before the PCB is recycled.
        let links = process.links_mut().drain();
        let watchers: Vec<MonitoredBy> = core::mem::take(process.monitored_by_mut());
        let outgoing = process.monitors_mut().drain();
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

        // 2. Monitors: every watcher is told the target is down.
        for watcher in watchers {
            if !self.contains(watcher.watcher) {
                // The watcher died earlier in this same propagation.
                continue;
            }
            self.enqueue_down_signal(
                watcher.watcher,
                watcher.reference,
                pid,
                watcher.kind,
                reason,
                Some(process.off_heap()),
            )?;
            receivers.push(watcher.watcher);
        }

        // 3. The dying process's own outgoing monitors vanish with it.
        for monitor in outgoing {
            if let Some(target) = self.get_mut(monitor.target) {
                target
                    .monitored_by_mut()
                    .retain(|entry| entry.reference != monitor.reference);
            }
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
    /// # Errors
    ///
    /// Propagates the message builders' `BeamError::Alloc` (after a collection) and the
    /// cascading termination's errors.
    pub fn service_signals(&mut self, pid: ProcessId) -> Result<()> {
        if !self.contains(pid) {
            return Ok(());
        }
        let mut pending = VecDeque::new();
        pending.push_back(pid);
        self.drain_signals(&mut pending)
    }

    /// Drains the signal queues of every pid in `pending`, and of any process the servicing
    /// itself signals, until no queue is left to drain.
    ///
    /// Iterative — the `pending` worklist is the only recursion the cascade has, so a link
    /// cycle (A↔B) terminates in a bounded number of steps.
    fn drain_signals(&mut self, pending: &mut VecDeque<ProcessId>) -> Result<()> {
        while let Some(pid) = pending.pop_front() {
            while self.apply_head_signal(pid, pending)? {}
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
                        self.terminate_into(pid, reason, pending)?;
                        Ok(false)
                    }
                }
            }
            PendingSignal::Down {
                reference,
                target,
                kind,
                ..
            } => {
                self.deliver_down_message(pid, reference, target, kind)?;
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

    /// Delivers the head (monitor-DOWN) signal of `pid` as a `{'DOWN', …}` message.
    fn deliver_down_message(
        &mut self,
        pid: ProcessId,
        reference: MonitorRef,
        target: ProcessId,
        kind: MonitorKind,
    ) -> Result<()> {
        self.deliver_head_signal(pid, |this, reason| {
            this.build_and_enqueue_down(pid, reference, target, kind, reason)
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

    /// Builds `{'DOWN', Ref, Tag, Target, reason}` on `pid`'s heap — `reason` already resident
    /// in that heap — and enqueues it as a message.
    fn build_and_enqueue_down(
        &mut self,
        pid: ProcessId,
        reference: MonitorRef,
        target: ProcessId,
        kind: MonitorKind,
        reason: Eterm,
    ) -> Result<()> {
        let atoms = &self.atoms;
        let Some(recipient) = lookup_mut(&mut self.slots, pid) else {
            return Ok(());
        };
        let tag = match kind {
            MonitorKind::Process => atoms.process(),
            MonitorKind::Port => atoms.port(),
        };
        let reference_term = reference.to_eterm(recipient.heap_mut())?;
        let term = build_down_message_term(
            recipient.heap_mut(),
            atoms,
            reference_term,
            tag,
            target.to_eterm(),
            reason,
        )?;
        recipient
            .mailbox_mut()
            .enqueue(Message::new(term, target.to_eterm()));
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
    /// described by `src` (see [`ProcessTable::enqueue_exit_signal`]).
    fn enqueue_down_signal(
        &mut self,
        to: ProcessId,
        reference: MonitorRef,
        target: ProcessId,
        kind: MonitorKind,
        reason: Eterm,
        src: Option<&OffHeapTable>,
    ) -> Result<()> {
        let Some(recipient) = lookup_mut(&mut self.slots, to) else {
            return Ok(());
        };
        let reason = copy_reason_onto(recipient, reason, src)?;
        recipient
            .signals_mut()
            .push(PendingSignal::down(reference, target, kind, reason));
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

    /// `erlang:link/1` against an arbitrary pid term: a local pid links through
    /// [`ProcessTable::link`], an external pid is recorded as a remote link and routed as a
    /// `DOP_LINK` control signal.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when `b` is not a pid; for a local `b`,
    /// propagates [`ProcessTable::link`]'s `BeamError::TermNotFound`; for a remote `b`,
    /// `BeamError::TermNotFound` when `a` is unknown; and the router's transport errors.
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

    /// `erlang:monitor/2` against an arbitrary pid term: a local pid monitors through
    /// [`ProcessTable::monitor`], an external pid is recorded as an outgoing remote monitor and
    /// routed as a `DOP_MONITOR_P` control signal.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when `target` is not a pid; for a local
    /// `target`, propagates [`ProcessTable::monitor`]'s `BeamError::TermNotFound`; for a remote
    /// `target`, `BeamError::TermNotFound` when `watcher` is unknown; and the router's transport
    /// errors.
    pub fn monitor_term(
        &mut self,
        watcher: ProcessId,
        target: Eterm,
        kind: MonitorKind,
    ) -> Result<MonitorRef> {
        match PidTarget::from_eterm(target)? {
            PidTarget::Local(pid) => self.monitor(watcher, pid, kind),
            PidTarget::Remote(remote) => {
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
                let term =
                    build_down_message_remote(heap, atoms, reference_term, remote, reason)?;
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

    /// Registers `pid` under `name`.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when the name is already taken.
    pub fn register(&mut self, name: Atom, pid: ProcessId) -> Result<()> {
        if self.names.contains_key(&name) {
            return Err(error::name_taken());
        }
        self.names.insert(name, pid);
        Ok(())
    }

    /// Removes `name` from the registry.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the name is not registered.
    pub fn unregister(&mut self, name: Atom) -> Result<()> {
        match self.names.remove(&name) {
            Some(_) => Ok(()),
            None => Err(error::unknown_process()),
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

        // 1. Monitors: every watcher is told the port is down.
        for monitor in closed.monitors {
            self.enqueue_port_down_message(monitor.watcher, monitor.reference, port, reason)?;
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
                ExitPolicy::Terminate(peer_reason) => self.terminate(pid, peer_reason)?,
            }
        }
        Ok(())
    }

    /// Enqueues `{'DOWN', Ref, port, Port, Reason}` into `to`'s mailbox — built on `to`'s own
    /// heap — and wakes `to` when it was blocked.
    ///
    /// An unknown `to` is skipped (the watcher died earlier).
    fn enqueue_port_down_message(
        &mut self,
        to: ProcessId,
        reference: MonitorRef,
        port: PortId,
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
                let term = build_down_message_port(heap, atoms, reference_term, port, reason)?;
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
        make_atom, make_small, tuple_element, tuple_size, AtomTable,
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

        assert!(matches!(
            table.link(a, ProcessId::new(999, 0)),
            Err(BeamError::TermNotFound)
        ));
        assert!(matches!(
            table.unlink(ProcessId::new(999, 0), a),
            Err(BeamError::TermNotFound)
        ));
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
        let mut table = table();
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

        assert!(matches!(
            table.monitor(watcher, ProcessId::new(999, 0), MonitorKind::Process),
            Err(BeamError::TermNotFound)
        ));
        assert!(matches!(
            table.demonitor(ProcessId::new(999, 0), reference),
            Err(BeamError::TermNotFound)
        ));
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
        assert_eq!(table.whereis(name), Some(pid));
        assert_eq!(table.whereis(other), None);

        let registered: Vec<(Atom, ProcessId)> = table.registered().collect();
        assert_eq!(registered, vec![(name, pid)]);

        table.unregister(name).expect("it was registered");
        assert!(matches!(
            table.unregister(name),
            Err(BeamError::TermNotFound)
        ));
        // A name registered to a dead process is not reported.
        table.register(name, pid2).expect("free again");
        table.remove(pid2);
        assert_eq!(table.registered().count(), 0);
    }
}
