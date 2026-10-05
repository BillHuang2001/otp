//! The process control block: [`Process`], its state, flags, message delivery and GC entry.
//!
//! `Process` is the PCB (the legacy C `Process` struct, `erts/emulator/beam/erl_process.h`)
//! reduced to what the single-scheduler Milestone-1 core needs: identity, scheduler state
//! and flags, the collector-owned heap, the mailbox, the registers/stack root set, the
//! reduction counter, the link/monitor sets and the termination reason.
//!
//! Nothing here is reimplemented from a sibling: the mailbox is `beam_message::Queue`, the
//! copy engine is `beam_message::send`/`receive`, the heaps are `beam_gc::Collector` over
//! `beam_alloc::Heap`, and the off-heap objects are `beam_message::OffHeapTable`.
//!
//! Legacy C ground truth (read-only): `erl_process.{c,h}` (the PCB and its state),
//! `erl_message.h` (`ERL_MESSAGE_*`) and `erl_gc.c` (`erts_garbage_collect`).

use beam_alloc::Heap;
use beam_common::{AllocKind, BeamError, Result};
use beam_gc::{Collection, Collector, HeapPolicy};
use beam_message::{copy_struct, CopyContext, Message, OffHeapTable, Queue};
use beam_term::prelude::Eterm;

use crate::link::Links;
use crate::monitor::{MonitoredBy, Monitors};
use crate::pid::ProcessId;
use crate::registers::Registers;

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

/// The process's boolean flags, as a `u32` bitset newtype (no `bitflags` dependency).
///
/// These are the flags the Milestone-1 core reads directly: `trap_exit`, `system`,
/// `in_run_queue`, `garbage_collecting` and `active`. The legacy C `F_*` word is far
/// wider; the rest is a follow-up.
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
    /// The two-generation collector owning the young (mutator) and mature heaps.
    collector: Collector,
    /// The process's off-heap (MSO) handles.
    off_heap: OffHeapTable,
    /// The mailbox.
    mailbox: Queue,
    /// The root-set containers (x-registers + Erlang stack).
    regs: Registers,
    /// Reductions consumed in the current time slice.
    reductions: u64,
    /// Outgoing links.
    links: Links,
    /// Outgoing monitors.
    monitors: Monitors,
    /// Incoming monitors (who monitors this process).
    monitored_by: Vec<MonitoredBy>,
    /// The termination reason, once the process has exited.
    exit_reason: Option<Eterm>,
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
    /// # Errors
    ///
    /// Propagates heap-allocation failure when the initial spaces cannot be allocated.
    pub fn with_policy(pid: ProcessId, policy: HeapPolicy) -> Result<Process> {
        // A freshly spawned process is runnable and schedulable: mark the `active` flag so
        // the rest of the runtime can tell a live PCB from a recycled slot.
        let mut flags = ProcessFlags::new();
        flags.set_active(true);
        Ok(Process {
            pid,
            state: ProcessState::Runnable,
            flags,
            collector: Collector::new(policy)?,
            off_heap: OffHeapTable::new(),
            mailbox: Queue::new(),
            regs: Registers::new(),
            reductions: 0,
            links: Links::new(),
            monitors: Monitors::new(),
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

    /// The process's off-heap (MSO) handles.
    pub fn off_heap(&self) -> &OffHeapTable {
        &self.off_heap
    }

    /// The process's off-heap (MSO) handles, mutably.
    pub fn off_heap_mut(&mut self) -> &mut OffHeapTable {
        &mut self.off_heap
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
    /// against `src_off_heap` — and enqueues it as a message from `from`.
    ///
    /// Retries once after a collection when the young heap (or the message table) is
    /// exhausted: `AllocKind::Heap`/`AllocKind::Message` exhaustion is a *normal*
    /// condition, not fatal OOM.
    ///
    /// # Errors
    ///
    /// Propagates the underlying `BeamError::Alloc` if the copy still does not fit after a
    /// collection.
    pub fn deliver(&mut self, from: Eterm, term: Eterm, src_off_heap: &OffHeapTable) -> Result<()> {
        match self.copy_message(from, term, src_off_heap) {
            Ok(()) => Ok(()),
            // A full young heap (or message table) is a normal condition: collect and
            // retry exactly once. Any other failure is propagated untouched.
            Err(BeamError::Alloc(failure))
                if matches!(failure.kind, AllocKind::Heap | AllocKind::Message) =>
            {
                self.collect()?;
                self.copy_message(from, term, src_off_heap)
            }
            Err(error) => Err(error),
        }
    }

    /// The no-off-heap convenience form of [`Process::deliver`].
    ///
    /// # Errors
    ///
    /// Propagates the underlying `BeamError::Alloc` if the copy still does not fit after a
    /// collection.
    pub fn deliver_simple(&mut self, from: Eterm, term: Eterm) -> Result<()> {
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
    pub fn collector(&self) -> &Collector {
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

    /// Runs a safepoint collection over the whole process root set.
    ///
    /// The root set is the registers/stack ([`Registers::gather_roots`]) **plus** the
    /// mailbox payloads (the queue is an in-heap sub-rootset): the queue is drained, its
    /// payloads appended to the root buffer, the collector runs, the registers are
    /// scattered back and each message is rebuilt from its relocated payload.
    ///
    /// # Errors
    ///
    /// Propagates the collector's and the rebuilding allocations' `BeamError`s.
    pub fn collect(&mut self) -> Result<Collection> {
        // 1. The flat root slice: `x[0..live)`, then the whole stack, then one word per
        //    queued mailbox payload. `reg_words` is where the mailbox roots start.
        let mut roots: Vec<Eterm> = Vec::new();
        let x_live = self.regs.gather_roots(&mut roots);
        let reg_words = roots.len();

        // 2. Drain the mailbox, appending each payload as a root. The messages themselves
        //    are rebuilt below, because `Message` has no payload setter.
        let mut drained: Vec<Message> = Vec::new();
        while let Some(message) = self.mailbox.dequeue() {
            roots.push(message.payload());
            drained.push(message);
        }

        // 3. Collect. On failure the messages must not be silently lost, so put them back
        //    before propagating (a collection of this heap cannot fail in practice: `collect`
        //    escalates a would-not-fit minor to a major, which sizes its to-space up front).
        let collection = match self.collector.collect(&mut roots) {
            Ok(collection) => collection,
            Err(error) => {
                for message in drained {
                    self.mailbox.enqueue(message);
                }
                return Err(error);
            }
        };

        // 4. Write the (relocated) registers and stack words back.
        self.regs.scatter_roots(&roots, x_live);

        // 5. Rebuild every message around its relocated payload, preserving FIFO order.
        let payloads = &roots[reg_words..];
        for (index, message) in drained.into_iter().enumerate() {
            let rebuilt = Message::new(payloads[index], message.from())
                .with_token(message.token())
                .with_off_heap(message.off_heap().to_vec());
            self.mailbox.enqueue(rebuilt);
        }

        Ok(collection)
    }

    /// Collects first when `need` more words do not fit, then reports whether a collection
    /// ran.
    ///
    /// # Errors
    ///
    /// Propagates the collector's errors.
    pub fn collect_if_needed(&mut self, need: usize) -> Result<bool> {
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
    /// Destructures `self` so the young heap, the mailbox and the off-heap table can be
    /// borrowed disjointly (a `&mut self` method cannot hold three simultaneous borrows).
    fn copy_message(&mut self, from: Eterm, term: Eterm, src_off_heap: &OffHeapTable) -> Result<()> {
        let Process {
            collector,
            mailbox,
            off_heap,
            ..
        } = self;
        let to_heap = collector.young_mut().heap_mut();
        // `send` drains `ctx`'s destination table into the message's off-heap list, so the
        // process's own table is the per-message off-heap table here.
        let mut ctx = CopyContext::new(src_off_heap, off_heap);
        beam_message::send(from, term, to_heap, mailbox, &mut ctx)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the PCB: `ProcessFlags` bits, the state/flag/reduction accessors,
    //! the link/monitor accessors and the collect policy. Message delivery and the full
    //! safepoint collection are exercised by the `tests/` integration suite.

    use beam_term::prelude::{make_small, AtomTable, NIL};

    use super::*;
    use crate::monitor::{Monitor, MonitorKind, RefAllocator};
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
    }

    #[test]
    fn collect_policy_tracks_the_young_heap_capacity() {
        let process = Process::new(pid(6)).expect("the default heap allocates");
        // A fresh nursery cannot satisfy an absurd request ...
        assert!(process.should_collect(usize::MAX / 2));
        // ... but a small one fits.
        assert!(!process.should_collect(1));
        assert_eq!(process.heap_used_words(), 0);
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
}
