//! The process/port table: pid allocation, the local name registry and the signal hub.
//!
//! [`ProcessTable`] is the **single place that mutates two or more processes at once** —
//! links, monitors and exit propagation inherently touch pairs of PCBs, and holding two
//! `&mut Process` out of one `HashMap` is a borrow-checker impossibility. Every such
//! operation therefore lives here and uses a **collect-then-apply worklist**: gather the
//! link targets / watchers, then mutate each target transiently. A [`Process`] itself
//! holds only its own sets (see the crate `CONTEXT.md`, Design Decisions).
//!
//! Milestone-1 is single-threaded, so the table owns the `Process` values directly and
//! propagates exit signals, links and monitors **synchronously** through a worklist inside
//! [`ProcessTable::terminate`]/[`ProcessTable::apply_exit_signal`], instead of the C's
//! decoupled per-process signal queue serviced at schedule-in — a documented follow-up.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_ptab.{c,h}` (the process/port
//! table and the local name registry), `erl_process.c` (`erts_ptab_new_process`) and
//! `erl_proc_sig_queue.c` (the signal queue being simplified away).

use std::collections::HashMap;

use beam_common::Result;
use beam_gc::HeapPolicy;
use beam_term::prelude::{Atom, Eterm};

use crate::monitor::{MonitorKind, MonitorRef, RefAllocator};
use crate::pid::{PortId, ProcessId};
use crate::process::Process;
use crate::signal::{ExitSignal, SignalAtoms};

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

/// The process table, the local name registry and the signal/link/monitor hub.
//
// TODO(impl): implement `ProcessTable` (all bodies are stubs).
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
}

impl ProcessTable {
    /// An empty table using `atoms` to build signal messages.
    pub fn new(atoms: SignalAtoms) -> ProcessTable {
        unimplemented!("stub — to be implemented")
    }

    // --- allocation / lookup -------------------------------------------------

    /// Allocates a fresh pid, reusing a free slot with a bumped serial.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::SystemLimit` when the pid number space is exhausted.
    pub fn alloc_pid(&mut self) -> Result<ProcessId> {
        unimplemented!("stub — to be implemented")
    }

    /// Allocates a pid and installs a fresh process with `policy`.
    ///
    /// # Errors
    ///
    /// Propagates pid allocation and initial heap allocation failures.
    pub fn spawn(&mut self, policy: HeapPolicy) -> Result<ProcessId> {
        unimplemented!("stub — to be implemented")
    }

    /// Allocates a pid and installs a fresh process with the default heap policy.
    ///
    /// # Errors
    ///
    /// Propagates pid allocation and initial heap allocation failures.
    pub fn spawn_default(&mut self) -> Result<ProcessId> {
        unimplemented!("stub — to be implemented")
    }

    /// Installs `process` in the slot its pid names.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::SystemLimit` when the pid's number is out of range.
    pub fn insert(&mut self, process: Process) -> Result<()> {
        unimplemented!("stub — to be implemented")
    }

    /// Borrows the process named by `pid`.
    pub fn get(&self, pid: ProcessId) -> Option<&Process> {
        unimplemented!("stub — to be implemented")
    }

    /// Borrows the process named by `pid`, mutably.
    pub fn get_mut(&mut self, pid: ProcessId) -> Option<&mut Process> {
        unimplemented!("stub — to be implemented")
    }

    /// Removes the process named by `pid`, leaving the slot free for reuse.
    pub fn take(&mut self, pid: ProcessId) -> Option<Process> {
        unimplemented!("stub — to be implemented")
    }

    /// Removes the process named by `pid`.
    pub fn remove(&mut self, pid: ProcessId) -> Option<Process> {
        unimplemented!("stub — to be implemented")
    }

    /// Whether `pid` names a live process.
    pub fn contains(&self, pid: ProcessId) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// The number of live processes.
    pub fn len(&self) -> usize {
        unimplemented!("stub — to be implemented")
    }

    /// Iterates the live pids.
    pub fn pids(&self) -> impl Iterator<Item = ProcessId> + '_ {
        crate::unimplemented_iter()
    }

    // --- signals / links / monitors -----------------------------------------

    /// Applies an exit signal to `target`, following [`crate::signal::exit_policy`].
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
        unimplemented!("stub — to be implemented")
    }

    /// Links `a` and `b` (both directions).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when either pid is unknown.
    pub fn link(&mut self, a: ProcessId, b: ProcessId) -> Result<()> {
        unimplemented!("stub — to be implemented")
    }

    /// Unlinks `a` and `b` (both directions).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when either pid is unknown.
    pub fn unlink(&mut self, a: ProcessId, b: ProcessId) -> Result<()> {
        unimplemented!("stub — to be implemented")
    }

    /// Sends an exit signal from `from` to `to` with `reason` (`erlang:exit/2`).
    ///
    /// # Errors
    ///
    /// Propagates [`ProcessTable::apply_exit_signal`]'s errors.
    pub fn send_exit(&mut self, from: ProcessId, to: ProcessId, reason: Eterm) -> Result<()> {
        unimplemented!("stub — to be implemented")
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
        unimplemented!("stub — to be implemented")
    }

    /// Removes the outgoing monitor `watcher` holds under `reference`, reporting whether it
    /// existed.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the watcher is unknown.
    pub fn demonitor(&mut self, watcher: ProcessId, reference: MonitorRef) -> Result<bool> {
        unimplemented!("stub — to be implemented")
    }

    /// Terminates `pid` with `reason`, propagating to its links and monitors and cascading
    /// through a worklist.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the pid is unknown; propagates the
    /// propagation's allocation and signalling errors.
    pub fn terminate(&mut self, pid: ProcessId, reason: Eterm) -> Result<()> {
        unimplemented!("stub — to be implemented")
    }

    // --- name registry -------------------------------------------------------

    /// Registers `pid` under `name`.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when the name is already taken.
    pub fn register(&mut self, name: Atom, pid: ProcessId) -> Result<()> {
        unimplemented!("stub — to be implemented")
    }

    /// Removes `name` from the registry.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the name is not registered.
    pub fn unregister(&mut self, name: Atom) -> Result<()> {
        unimplemented!("stub — to be implemented")
    }

    /// The process registered under `name`, if any.
    pub fn whereis(&self, name: Atom) -> Option<ProcessId> {
        unimplemented!("stub — to be implemented")
    }

    /// Iterates the registered `(name, pid)` pairs.
    pub fn registered(&self) -> impl Iterator<Item = (Atom, ProcessId)> + '_ {
        crate::unimplemented_iter()
    }
}

/// The minimal port-id table: allocation and ownership only.
///
/// Port *behaviour* (scheduling, port signals, port links/monitors) is `beam_io`'s; this is
/// only the number table the legacy C keeps alongside the pid table.
//
// TODO(impl): implement `PortTable` (all bodies are stubs).
pub struct PortTable {
    /// Number → whether the slot is in use.
    slots: Vec<bool>,
    /// Free slot numbers available for reuse.
    free: Vec<u64>,
}

impl PortTable {
    /// An empty port table.
    pub fn new() -> PortTable {
        unimplemented!("stub — to be implemented")
    }

    /// Allocates a fresh port id.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::SystemLimit` when the port number space is exhausted.
    pub fn alloc(&mut self) -> Result<PortId> {
        unimplemented!("stub — to be implemented")
    }

    /// Releases `id`, returning its number to the free list.
    pub fn release(&mut self, id: PortId) {
        unimplemented!("stub — to be implemented")
    }

    /// Whether `id` is currently allocated.
    pub fn contains(&self, id: PortId) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// The number of allocated ports.
    pub fn len(&self) -> usize {
        unimplemented!("stub — to be implemented")
    }
}
