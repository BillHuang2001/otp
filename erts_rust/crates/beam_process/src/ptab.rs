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

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_gc::HeapPolicy;
use beam_message::Message;
use beam_term::prelude::{Atom, Eterm, PORT_DATA_SIZE};

use crate::error;
use crate::monitor::{Monitor, MonitorKind, MonitorRef, MonitoredBy, RefAllocator};
use crate::pid::{PortId, ProcessId, MAX_PROCESS_NUMBER};
use crate::process::{Process, ProcessState};
use crate::signal::{build_down_message, build_exit_message, exit_policy, ExitPolicy, ExitSignal, SignalAtoms};

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

/// Builds a signal term on `process`'s heap, collecting once and retrying when the heap is
/// exhausted (a full heap is a normal condition, not fatal OOM).
fn build_signal_term<F>(process: &mut Process, build: F) -> Result<Eterm>
where
    F: Fn(&mut Heap) -> Result<Eterm>,
{
    match build(process.heap_mut()) {
        Ok(term) => Ok(term),
        Err(BeamError::Alloc(_)) => {
            process.collect()?;
            build(process.heap_mut())
        }
        Err(error) => Err(error),
    }
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
        }
    }

    // --- allocation / lookup -------------------------------------------------

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
        let Some(process) = self.get(target) else {
            return Err(error::unknown_process());
        };
        let trapping = process.is_trapping_exit();
        match exit_policy(signal.reason, trapping, &self.atoms) {
            ExitPolicy::Ignore => Ok(ExitEffect::Ignored),
            ExitPolicy::Trap => {
                self.enqueue_exit_message(target, signal.from, signal.reason)?;
                Ok(ExitEffect::Trapped)
            }
            ExitPolicy::Terminate(reason) => {
                self.terminate(target, reason)?;
                Ok(ExitEffect::Terminated)
            }
        }
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
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the watcher is unknown.
    pub fn demonitor(&mut self, watcher: ProcessId, reference: MonitorRef) -> Result<bool> {
        let Some(process) = self.get_mut(watcher) else {
            return Err(error::unknown_process());
        };
        let Some(monitor) = process.monitors_mut().remove(reference) else {
            return Ok(false);
        };
        // Drop the matching incoming entry from the (still living) target.
        if let Some(target) = self.get_mut(monitor.target) {
            target
                .monitored_by_mut()
                .retain(|entry| entry.reference != reference);
        }
        Ok(true)
    }

    /// Terminates `pid` with `reason`, propagating to its links and monitors and cascading
    /// through a worklist.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the pid is unknown; propagates the
    /// propagation's allocation and signalling errors.
    pub fn terminate(&mut self, pid: ProcessId, reason: Eterm) -> Result<()> {
        if !self.contains(pid) {
            return Err(error::unknown_process());
        }

        // A cascading termination is a worklist, never recursion: a link cycle (A↔B) then
        // terminates in a bounded number of steps instead of recursing forever.
        let mut worklist: Vec<(ProcessId, Eterm)> = vec![(pid, reason)];

        while let Some((pid, reason)) = worklist.pop() {
            // A process can be signalled twice within one propagation (two links into the
            // same target); the second visit finds the slot already reclaimed and is a
            // no-op.
            let Some(mut process) = self.take(pid) else {
                continue;
            };

            // A dying process keeps no name registration.
            self.names.retain(|_, registered| *registered != pid);

            // Snapshot every relationship before the PCB is recycled.
            let links = process.links_mut().drain();
            let watchers: Vec<MonitoredBy> = core::mem::take(process.monitored_by_mut());
            let outgoing = process.monitors_mut().drain();
            process.set_exit_reason(reason);
            process.set_state(ProcessState::Terminated);
            drop(process);

            // 1. Links: drop the back-link, then either trap the signal on the peer or
            //    queue the peer's termination.
            for target in links {
                if let Some(peer) = self.get_mut(target) {
                    peer.links_mut().remove(pid);
                }
                let Some(peer) = self.get(target) else {
                    // The peer died earlier in this same propagation.
                    continue;
                };
                let trapping = peer.is_trapping_exit();
                match exit_policy(reason, trapping, &self.atoms) {
                    ExitPolicy::Ignore => {}
                    ExitPolicy::Trap => self.enqueue_exit_message(target, pid, reason)?,
                    ExitPolicy::Terminate(peer_reason) => {
                        worklist.push((target, peer_reason));
                    }
                }
            }

            // 2. Monitors: every watcher is told the target is down.
            for watcher in watchers {
                self.enqueue_down_message(watcher.watcher, watcher.reference, pid, reason)?;
            }

            // 3. The dying process's own outgoing monitors vanish with it.
            for monitor in outgoing {
                if let Some(target) = self.get_mut(monitor.target) {
                    target
                        .monitored_by_mut()
                        .retain(|entry| entry.reference != monitor.reference);
                }
            }
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

    /// Enqueues `{'EXIT', from, reason}` into `to`'s mailbox, building it on `to`'s heap.
    ///
    /// An unknown `to` is skipped: the caller is propagating a signal through the table and
    /// the target may have died earlier in the same propagation.
    fn enqueue_exit_message(&mut self, to: ProcessId, from: ProcessId, reason: Eterm) -> Result<()> {
        let atoms = &self.atoms;
        let Some(process) = lookup_mut(&mut self.slots, to) else {
            return Ok(());
        };
        let term =
            build_signal_term(process, |heap| build_exit_message(heap, atoms, from, reason))?;
        process
            .mailbox_mut()
            .enqueue(Message::new(term, from.to_eterm()));
        Ok(())
    }

    /// Enqueues `{'DOWN', reference, process, pid, reason}` into `to`'s mailbox, building it
    /// on `to`'s heap.
    ///
    /// An unknown `to` is skipped (the watcher died earlier in the same propagation).
    fn enqueue_down_message(
        &mut self,
        to: ProcessId,
        reference: MonitorRef,
        pid: ProcessId,
        reason: Eterm,
    ) -> Result<()> {
        let atoms = &self.atoms;
        let Some(process) = lookup_mut(&mut self.slots, to) else {
            return Ok(());
        };
        let term = build_signal_term(process, |heap| {
            let reference_term = reference.to_eterm(heap)?;
            build_down_message(heap, atoms, reference_term, pid, reason)
        })?;
        process
            .mailbox_mut()
            .enqueue(Message::new(term, pid.to_eterm()));
        Ok(())
    }
}

/// The minimal port-id table: allocation and ownership only.
///
/// Port *behaviour* (scheduling, port signals, port links/monitors) is `beam_io`'s; this is
/// only the number table the legacy C keeps alongside the pid table.
pub struct PortTable {
    /// Number → whether the slot is in use.
    slots: Vec<bool>,
    /// Free slot numbers available for reuse.
    free: Vec<u64>,
}

impl PortTable {
    /// An empty port table.
    pub fn new() -> PortTable {
        PortTable {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    /// Allocates a fresh port id.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::SystemLimit` when the port number space is exhausted.
    pub fn alloc(&mut self) -> Result<PortId> {
        if let Some(number) = self.free.pop() {
            if let Some(slot) = self.slots.get_mut(number as usize) {
                *slot = true;
            }
            return Ok(PortId::new(number));
        }
        let number = self.slots.len() as u64;
        // The whole 60-bit immediate payload is the port number (`_PORT_DATA_SIZE`).
        if number > (1u64 << PORT_DATA_SIZE) - 1 {
            return Err(error::system_limit());
        }
        self.slots.push(true);
        Ok(PortId::new(number))
    }

    /// Releases `id`, returning its number to the free list.
    pub fn release(&mut self, id: PortId) {
        let number = id.number();
        if let Some(slot) = self.slots.get_mut(number as usize) {
            if *slot {
                *slot = false;
                self.free.push(number);
            }
        }
    }

    /// Whether `id` is currently allocated.
    pub fn contains(&self, id: PortId) -> bool {
        self.slots
            .get(id.number() as usize)
            .copied()
            .unwrap_or(false)
    }

    /// The number of allocated ports.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|&&used| used).count()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for pid allocation/serial bumping/stale-pid rejection, the name
    //! registry, link/unlink, the exit-signal outcomes, monitor/demonitor and the minimal
    //! port table. The cascading link/monitor propagation is covered by `tests/signals.rs`.

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
                .apply_exit_signal(target, ExitSignal { from, reason: normal })
                .expect("a live target"),
            ExitEffect::Ignored
        );
        assert!(table.contains(target));

        // A non-atom "other" reason: to a non-trapping target it terminates.
        let other = make_small(1234);
        assert_eq!(
            table
                .apply_exit_signal(target, ExitSignal { from, reason: other })
                .expect("a live target"),
            ExitEffect::Terminated
        );
        assert!(!table.contains(target));

        // A trapping target gets `{'EXIT', from, reason}` and survives.
        let target = table.spawn_default().expect("target");
        table.get_mut(target).expect("target").set_trap_exit(true);
        assert_eq!(
            table
                .apply_exit_signal(target, ExitSignal { from, reason: other })
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
            table.get(watcher).expect("watcher").monitors().find(reference).map(|m| m.target),
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
        assert_eq!(table.whereis(name), None, "a name whose process is gone is gone");
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
        assert!(matches!(
            table.register(name, pid2),
            Err(BeamError::BadArg)
        ));
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

    #[test]
    fn port_table_allocates_releases_and_reuses() {
        let mut ports = PortTable::new();
        assert_eq!(ports.len(), 0);

        let a = ports.alloc().expect("room");
        let b = ports.alloc().expect("room");
        assert_ne!(a, b);
        assert_eq!(ports.len(), 2);
        assert!(ports.contains(a) && ports.contains(b));

        ports.release(a);
        assert!(!ports.contains(a));
        assert_eq!(ports.len(), 1);

        let reused = ports.alloc().expect("the freed number is reused");
        assert_eq!(reused, a);
        assert!(ports.contains(reused));
        assert_eq!(ports.len(), 2);

        // Releasing twice does not double-free.
        ports.release(b);
        ports.release(b);
        assert_eq!(ports.len(), 1);
    }
}
