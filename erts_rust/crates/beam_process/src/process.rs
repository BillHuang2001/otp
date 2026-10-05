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
use beam_common::Result;
use beam_gc::{Collection, Collector, HeapPolicy};
use beam_message::{Message, OffHeapTable, Queue};
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
//
// TODO(impl): implement `ProcessFlags` (all bodies are stubs).
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
        unimplemented!("stub — to be implemented")
    }

    /// The raw bitset.
    pub fn bits(&self) -> u32 {
        unimplemented!("stub — to be implemented")
    }

    /// Rebuilds the flags from a raw bitset.
    pub fn from_bits(bits: u32) -> ProcessFlags {
        unimplemented!("stub — to be implemented")
    }

    /// Whether the process traps exit signals (`process_flag(trap_exit, true)`).
    pub fn trap_exit(&self) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Sets the `trap_exit` flag.
    pub fn set_trap_exit(&mut self, value: bool) {
        unimplemented!("stub — to be implemented")
    }

    /// Whether the process belongs to a system/OS process (`system` flag).
    pub fn system(&self) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Sets the `system` flag.
    pub fn set_system(&mut self, value: bool) {
        unimplemented!("stub — to be implemented")
    }

    /// Whether the process is currently enqueued in a run queue.
    pub fn in_run_queue(&self) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Sets the `in_run_queue` flag.
    pub fn set_in_run_queue(&mut self, value: bool) {
        unimplemented!("stub — to be implemented")
    }

    /// Whether the process is currently being garbage collected.
    pub fn garbage_collecting(&self) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Sets the `garbage_collecting` flag.
    pub fn set_garbage_collecting(&mut self, value: bool) {
        unimplemented!("stub — to be implemented")
    }

    /// Whether the process is active (schedulable at all).
    pub fn active(&self) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Sets the `active` flag.
    pub fn set_active(&mut self, value: bool) {
        unimplemented!("stub — to be implemented")
    }
}

/// The process control block (PCB).
///
/// Owns the process's identity, scheduler state, heap (via the collector), mailbox,
/// registers, reduction counter, links/monitors and exit reason. Signal delivery and GC
/// entry also live here; everything that mutates **two** processes at once lives on
/// [`crate::ptab::ProcessTable`] instead (see the crate `CONTEXT.md`, Design Decisions).
//
// TODO(impl): implement `Process` (all bodies are stubs).
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
        unimplemented!("stub — to be implemented")
    }

    /// A new process with pid `pid` and an explicit heap-size `policy`.
    ///
    /// # Errors
    ///
    /// Propagates heap-allocation failure when the initial spaces cannot be allocated.
    pub fn with_policy(pid: ProcessId, policy: HeapPolicy) -> Result<Process> {
        unimplemented!("stub — to be implemented")
    }

    /// This process's pid.
    pub fn pid(&self) -> ProcessId {
        unimplemented!("stub — to be implemented")
    }

    /// The scheduler-visible state.
    pub fn state(&self) -> ProcessState {
        unimplemented!("stub — to be implemented")
    }

    /// Sets the scheduler-visible state.
    pub fn set_state(&mut self, state: ProcessState) {
        unimplemented!("stub — to be implemented")
    }

    /// The process's flags.
    pub fn flags(&self) -> &ProcessFlags {
        unimplemented!("stub — to be implemented")
    }

    /// The process's flags, mutably.
    pub fn flags_mut(&mut self) -> &mut ProcessFlags {
        unimplemented!("stub — to be implemented")
    }

    /// Whether the process traps exit signals (`trap_exit`).
    pub fn is_trapping_exit(&self) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// Sets the `trap_exit` flag.
    pub fn set_trap_exit(&mut self, value: bool) {
        unimplemented!("stub — to be implemented")
    }

    /// Reductions consumed in the current time slice.
    pub fn reductions(&self) -> u64 {
        unimplemented!("stub — to be implemented")
    }

    /// Adds `n` to the reduction counter.
    pub fn bump_reductions(&mut self, n: u64) {
        unimplemented!("stub — to be implemented")
    }

    /// Resets the reduction counter to zero.
    pub fn reset_reductions(&mut self) {
        unimplemented!("stub — to be implemented")
    }

    /// The young (mutator) heap — where this process allocates terms.
    pub fn heap(&self) -> &Heap {
        unimplemented!("stub — to be implemented")
    }

    /// The young (mutator) heap, mutably.
    pub fn heap_mut(&mut self) -> &mut Heap {
        unimplemented!("stub — to be implemented")
    }

    /// The mailbox.
    pub fn mailbox(&self) -> &Queue {
        unimplemented!("stub — to be implemented")
    }

    /// The mailbox, mutably.
    pub fn mailbox_mut(&mut self) -> &mut Queue {
        unimplemented!("stub — to be implemented")
    }

    /// The root-set containers.
    pub fn registers(&self) -> &Registers {
        unimplemented!("stub — to be implemented")
    }

    /// The root-set containers, mutably.
    pub fn registers_mut(&mut self) -> &mut Registers {
        unimplemented!("stub — to be implemented")
    }

    /// The process's off-heap (MSO) handles.
    pub fn off_heap(&self) -> &OffHeapTable {
        unimplemented!("stub — to be implemented")
    }

    /// The process's off-heap (MSO) handles, mutably.
    pub fn off_heap_mut(&mut self) -> &mut OffHeapTable {
        unimplemented!("stub — to be implemented")
    }

    /// The outgoing links.
    pub fn links(&self) -> &Links {
        unimplemented!("stub — to be implemented")
    }

    /// The outgoing links, mutably.
    pub fn links_mut(&mut self) -> &mut Links {
        unimplemented!("stub — to be implemented")
    }

    /// The outgoing monitors.
    pub fn monitors(&self) -> &Monitors {
        unimplemented!("stub — to be implemented")
    }

    /// The outgoing monitors, mutably.
    pub fn monitors_mut(&mut self) -> &mut Monitors {
        unimplemented!("stub — to be implemented")
    }

    /// The monitors watching this process.
    pub fn monitored_by(&self) -> &[MonitoredBy] {
        unimplemented!("stub — to be implemented")
    }

    /// The monitors watching this process, mutably.
    pub fn monitored_by_mut(&mut self) -> &mut Vec<MonitoredBy> {
        unimplemented!("stub — to be implemented")
    }

    /// The termination reason, once the process has exited.
    pub fn exit_reason(&self) -> Option<Eterm> {
        unimplemented!("stub — to be implemented")
    }

    /// Sets the termination reason.
    pub fn set_exit_reason(&mut self, reason: Eterm) {
        unimplemented!("stub — to be implemented")
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
        unimplemented!("stub — to be implemented")
    }

    /// The no-off-heap convenience form of [`Process::deliver`].
    ///
    /// # Errors
    ///
    /// Propagates the underlying `BeamError::Alloc` if the copy still does not fit after a
    /// collection.
    pub fn deliver_simple(&mut self, from: Eterm, term: Eterm) -> Result<()> {
        unimplemented!("stub — to be implemented")
    }

    /// Removes and returns the first queued message whose payload satisfies `pred`.
    ///
    /// Non-matching messages stay queued in FIFO order (selective receive). Returns `None`
    /// when nothing matches — actually *waiting* is the scheduler's job.
    pub fn receive<F: FnMut(&Message) -> bool>(&mut self, pred: F) -> Option<Message> {
        unimplemented!("stub — to be implemented")
    }

    /// The process's garbage collector.
    pub fn collector(&self) -> &Collector {
        unimplemented!("stub — to be implemented")
    }

    /// Whether the young heap can no longer satisfy `need_words` more words.
    pub fn should_collect(&self, need_words: usize) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// The number of words used in the young heap.
    pub fn heap_used_words(&self) -> usize {
        unimplemented!("stub — to be implemented")
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
        unimplemented!("stub — to be implemented")
    }

    /// Collects first when `need` more words do not fit, then reports whether a collection
    /// ran.
    ///
    /// # Errors
    ///
    /// Propagates the collector's errors.
    pub fn collect_if_needed(&mut self, need: usize) -> Result<bool> {
        unimplemented!("stub — to be implemented")
    }
}
