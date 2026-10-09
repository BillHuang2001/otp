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

//! **Owner-death reclamation** — the runtime's host for the `beam_process`
//! `ProcessExitObserver` seam, carrying the two obligations a death leaves behind:
//!
//! * **ETS tables** (`erl_db.c`, contract A.1). An ETS table belongs to the process that
//!   created it, so the runtime must release it the moment that process dies — the table
//!   disappears, or it is handed to its installed heir (`beam_ets::Table::owner_died`).
//!   [`Runtime::drain_ets_owner_exits`] discharges that obligation.
//! * **Dangling port monitors** (`erl_ptab.c`, contract A.4). `erlang:monitor(port, Port)`
//!   records an *incoming* monitor on the port, so a port close can deliver
//!   `{'DOWN', Ref, port, Port, Reason}` to the watcher. When the **watcher** dies first, that
//!   incoming entry outlives it — a port would keep a monitor for a process that no longer
//!   exists. [`Runtime::drain_port_monitor_exits`] sweeps it.
//!
//! `beam_process` cannot know what the ETS registry allocated on a pid's behalf, and this
//! runtime threads no [`PortTable`](beam_process::PortTable) into the termination path, so it
//! exposes the [`ProcessExitObserver`](beam_process::ProcessExitObserver) seam instead: the
//! runtime installs an observer on its [`ProcessTable`] and is called **exactly once per
//! death**, from inside the finalizer, before the dying heap is released.
//!
//! # Why the observer only *records*
//!
//! The finalizer can run while a `RefCell` the runtime owns is already borrowed — a BIF call
//! holds the shared ETS [`TableRegistry`](beam_bifs::TableRegistry) (or the shared port
//! registry) for the whole of its call, and a death inside that call would be reached with the
//! cell taken. An observer that reached for either there would either panic or silently lose
//! the obligation. So the observer touches **nothing but its own** [`OwnerIndex`] cell (a cell
//! no BIF ever borrows): it pushes the dead pid and the drained
//! [`PortWatch`](beam_process::PortWatch) pairs and returns. The two drains then discharge the
//! recorded obligations at a point where no such borrow is live — a scheduler-pass round —
//! using `try_borrow_mut` and **deferring** (never dropping, never panicking) when the registry
//! is still held.
//!
//! # Which tables a death reclaims
//!
//! The **authority** is `beam_bifs::TableRegistry::handles_owned_by` — the public owner→handle
//! roster. It lists every live table whose recorded owner is the dead pid, so it covers an
//! **unnamed** table too (whose handle is the registry key and is reachable from no public term
//! otherwise). The index recorded at `ets:new/2` time is kept only for what the roster cannot
//! give: the **registered name** of a `named_table`, which must be released with
//! [`TableRegistry::remove_name`] when the table is dropped (a table's *name* can only be looked
//! up from a name one already knows, and an unnamed table has none).
//!
//! A marked table is dropped, or handed to a live heir with the oracle's
//! `{'ETS-TRANSFER', Tid, FromPid, Data}` message. The tid is the oracle's in each case: the
//! table's **name atom** for a `named_table`, and the table's **reference** for an unnamed one
//! (see [`tid_ref_thing`] for how this unsafe-free crate materializes that frozen thing).
//!
//! # The unsubscribe gap (a handoff, never a wrong result)
//!
//! `ets:setopts(T, {heir, …})`/`ets:give_away/3` move a table without the runtime observing it,
//! and a table's *name* discovered after creation (a `named_table` renamed by no one, but whose
//! creation the runtime did not see) would leave a name registration behind. That is a name
//! leak, not a table leak: the table itself is still reclaimed through the roster.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_bifs::TableRegistry;
use beam_common::Result;
use beam_ets::{OwnerDeath, OwnerId};
use beam_process::{
    MonitorRef, PortId, PortWatch, ProcessExit, ProcessExitObserver, ProcessId, ProcessTable,
    RefAllocator, Scheduler,
};
use beam_term::{make_atom, Atom, Eterm};

use super::Runtime;

/// The index key of an owner: the raw value of its internal pid immediate.
///
/// The same encoding `beam_bifs`'s `ets` resolver uses for `OwnerId`
/// (`OwnerId::new(pid.to_eterm().raw() as u64)`), so an owner read back from a table and an
/// owner recorded here always agree.
pub(crate) fn owner_key(pid: ProcessId) -> u64 {
    pid.to_eterm().raw() as u64
}

/// One `named_table` a live process created: its registry handle and its name atom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OwnedTable {
    /// The `TableRegistry` handle (an `ets:new/2`-time `TableId`).
    pub(crate) handle: u64,
    /// The table's `named_table` name — also the tid the oracle puts in an
    /// `{'ETS-TRANSFER', …}` message for a named table.
    pub(crate) name: Atom,
}

/// The owner→named-table index plus the death obligations waiting to be discharged.
///
/// Shared through an [`Rc`]`<`[`RefCell`]`>` between the [`Runtime`] (which records creations
/// and drains) and the [`EtsExitObserver`] (which records deaths). No BIF ever borrows this
/// cell, so the observer can always take it — that is what makes the observer safe to run from
/// inside the finalizer while an ETS (or port) borrow is live.
#[derive(Default)]
pub(crate) struct OwnerIndex {
    /// The `named_table`s each live owner created, keyed by [`owner_key`].
    tables: HashMap<u64, Vec<OwnedTable>>,
    /// The pids observed dead, in death order, waiting for a drain.
    deaths: Vec<ProcessId>,
    /// The dangling **incoming port monitors** the dead watchers left behind, as
    /// `(port, reference)` pairs, waiting for a drain.
    ///
    /// Recorded straight from the observer's
    /// [`ProcessExit::port_monitors`](beam_process::ProcessExit::port_monitors) payload — a
    /// [`PortWatch`] is a pair of plain values, so nothing has to be copied out of the dying
    /// heap. Each pair is the `reference` of an `erlang:monitor(port, Port)` watch recorded on
    /// the port table (see [`crate::port`]), which this drain removes so the port keeps no
    /// monitor for a watcher that no longer exists.
    port_watches: Vec<(PortId, MonitorRef)>,
}

impl OwnerIndex {
    /// Records that `owner` created the `named_table` under `handle` with name `name`.
    ///
    /// Idempotent per `(owner, handle)`: a re-observation of the same creation never
    /// duplicates an entry.
    pub(crate) fn record(&mut self, owner: ProcessId, handle: u64, name: Atom) {
        let entry = self.tables.entry(owner_key(owner)).or_default();
        if !entry.iter().any(|t| t.handle == handle) {
            entry.push(OwnedTable { handle, name });
        }
    }

    /// Records that a dead process was watching each of `watches` (its drained outgoing port
    /// monitors), so the matching incoming entries on the port table can be swept.
    ///
    /// Append-only: two deaths that watched the same port are two separate obligations, and a
    /// reference is unique to one watch, so no de-duplication is needed (or wanted).
    pub(crate) fn record_port_watches(&mut self, watches: &[PortWatch]) {
        self.port_watches
            .extend(watches.iter().map(|watch| (watch.port, watch.reference)));
    }

    /// Takes the recorded dangling port monitors out for a drain, leaving the cell empty.
    pub(crate) fn take_port_watches(&mut self) -> Vec<(PortId, MonitorRef)> {
        std::mem::take(&mut self.port_watches)
    }

    /// The number of owners with at least one indexed table (test/diagnostic).
    #[cfg(test)]
    pub(crate) fn owner_count(&self) -> usize {
        self.tables.len()
    }

    /// The number of deaths waiting for a drain (test/diagnostic).
    #[cfg(test)]
    pub(crate) fn pending_deaths(&self) -> usize {
        self.deaths.len()
    }

    /// The number of dangling port monitors waiting for a drain (test/diagnostic).
    #[cfg(test)]
    pub(crate) fn pending_port_watches(&self) -> usize {
        self.port_watches.len()
    }
}

/// The `ProcessExitObserver` host: records the dead pid and its drained port watches, and
/// nothing else.
///
/// The `reason` is deliberately **not** retained (it may live in the dying heap, and no part
/// of either obligation needs it), so there is nothing to copy. The
/// [`PortWatch`](beam_process::PortWatch) pairs *are* retained as they are: a
/// [`PortId`](beam_process::PortId) and a `MonitorRef` are plain values, so the slice the
/// finalizer hands over stays valid after the dying heap is released (see
/// [`ProcessExit`](beam_process::ProcessExit)).
pub(crate) struct EtsExitObserver {
    /// The shared index the runtime drains.
    index: Rc<RefCell<OwnerIndex>>,
}

impl EtsExitObserver {
    /// An observer recording into `index`.
    pub(crate) fn new(index: Rc<RefCell<OwnerIndex>>) -> EtsExitObserver {
        EtsExitObserver { index }
    }
}

impl ProcessExitObserver for EtsExitObserver {
    fn process_exited(&mut self, exit: &ProcessExit<'_>) {
        // Only the pid and the port watches are retained: both are heap-independent (a pid and
        // a `PortWatch` are plain values), and together they are the whole obligation — the
        // affected ETS tables are looked up at drain time. The borrow is taken once and never
        // held across the call.
        let mut index = self.index.borrow_mut();
        index.deaths.push(exit.pid());
        index.record_port_watches(exit.port_monitors());
    }
}
/// Whether an owner read back from a table is still a live process.
///
/// An owner that does not decode to an internal pid (never produced by `ets:new/2` here) can
/// own nothing this runtime reclaimed, so it is reported not-alive — the conservative answer
/// for `owner_died`'s heir check.
fn owner_is_alive(table: &ProcessTable, owner: OwnerId) -> bool {
    match ProcessId::from_eterm(Eterm::from_raw(owner.raw() as usize)) {
        Ok(pid) => table.contains(pid),
        Err(_) => false,
    }
}

/// The **tid** the oracle puts in `{'ETS-TRANSFER', Tid, FromPid, Data}` for an *unnamed*
/// table: the table's reference — this runtime's frozen 4-word ref thing
/// `[make_header(3, REF_SUBTAG), 0, 0, id]` whose `id` **is the registry handle** (the
/// table-taking BIFs decode a tid with `beam_bifs::access::ref_thing_id`, i.e. the last payload
/// word, so a tid that does not decode back to the handle must never be built).
///
/// # Why this goes through [`MonitorRef`]
///
/// The only producer of that frozen thing is `beam_bifs::access::make_ref_thing`, which is
/// `pub(crate)`, and [`MonitorRef`] has no public `from_id` constructor (`id` is private; only
/// [`RefAllocator::next`] mints one). [`MonitorRef::to_eterm`] is byte-identical to
/// `make_ref_thing` (both write `[make_header(3, REF_SUBTAG), 0, 0, id]`), so it is the
/// **substitution** used here: a fresh [`RefAllocator`] hands out ids `0, 1, 2, …`, so `handle`
/// is reached after `handle + 1` draws.
///
/// The caveat is the cost: the draw count is the handle value. That is bounded by the number of
/// tables this OS process has created (`beam_ets`'s `TableId` counter is global-monotonic from
/// `1`), and this path only runs for an unnamed table that carries an installed arity-3 heir,
/// so it is small in every real run — but it is *not* O(1). A public mint — widen
/// `beam_bifs::access::make_ref_thing` to `pub`, or add `MonitorRef::from_id(u64)` — removes the
/// loop; both are cross-crate changes reported as a handoff.
///
/// Using a fresh allocator (rather than the runtime's own [`Runtime::refs_mut`]) is deliberate:
/// the runtime's ref space hands out *its own* next id, which is **not** the handle, so the tid
/// would not resolve back to the table — a wrong tid, worse than any substitute.
///
/// # Errors
///
/// Propagates [`BeamError::Alloc`](beam_common::BeamError::Alloc) when `heap` cannot fit the
/// four words.
fn tid_ref_thing(heap: &mut Heap, handle: u64) -> Result<Eterm> {
    let mut refs = RefAllocator::new();
    let mut reference = refs.next(); // id 0
    for _ in 0..handle {
        reference = refs.next();
    }
    reference.to_eterm(heap)
}

impl Runtime {
    /// Adopts a `named_table` this runtime's `ets:new/2` **just created** for `owner`: records
    /// the `name` registration against its `handle` — the one thing
    /// [`Runtime::drain_ets_owner_exits`] cannot recover from the registry afterwards, since a
    /// table's name can only be looked up from a name one already knows — and takes ownership of
    /// the creation-time heir's gift term.
    ///
    /// Called from the runtime's `ets:new/2` interception
    /// ([`RuntimeContext::call_bif`](crate::context::RuntimeContext::call_bif)) with the handle
    /// the creation newly registered under `name` — the one point where the runtime, which
    /// cannot see `beam_bifs`'s private table registry, can resolve a fresh table's handle.
    /// (The *set* of tables a death reclaims no longer needs this: it comes from the registry's
    /// owner→handle roster, so an unnamed table is covered too.)
    pub(crate) fn adopt_created_table(&mut self, owner: ProcessId, handle: u64, name: Atom) {
        self.ets_owners.borrow_mut().record(owner, handle, name);
        self.own_heir_gift(handle);
    }

    /// Makes a freshly created table's heir gift term a **table-owned copy**.
    ///
    /// A creation-time `{heir, Pid, Data}` reaches the table as a raw term living in the
    /// **creating process's** heap (`beam_bifs`'s `ets:new/2` installs the parsed heir with
    /// [`Table::set_heir`](beam_ets::Table::set_heir), the non-copying primitive), so it
    /// aliases that heap: the moment the creator dies — or its per-process heap is collected
    /// over — the pointer dangles and the `{'ETS-TRANSFER', …, Data}` message would carry
    /// garbage. The oracle instead stores the gift data in a table-owned term
    /// (`copy_heir_data`, `erl_db.c`), which is what
    /// [`Table::install_heir`](beam_ets::Table::install_heir) reproduces (the A.1 copy
    /// boundary). This runs while the creator is still alive, so the copy reads a live term.
    ///
    /// A term outside `beam_ets`'s copyable subset (a fun, an off-heap bin ref, …) is that
    /// crate's documented typed `badarg`; the raw reference is then left in place — the
    /// behaviour from before this copy existed — so a gift term this runtime cannot own never
    /// turns a working `ets:new/2` into a failure. (The oracle copies *any* term, so closing
    /// that subset is a `beam_ets` handoff, not a wrong result here.)
    fn own_heir_gift(&mut self, handle: u64) {
        // The registry is free here (this runs right after the `ets:new/2` BIF returned), but a
        // `try_borrow_mut` keeps a nested borrow a no-op instead of a panic — the same
        // discipline [`Runtime::drain_ets_owner_exits`] follows.
        let Ok(mut registry) = self.tables.try_borrow_mut() else {
            return;
        };
        let Some(table) = registry.get_mut(handle) else {
            return;
        };
        let Some(heir) = table.heir() else {
            return;
        };
        let Some(data) = heir.data else {
            // An arity-2 `{heir, Pid}` owns no gift storage — nothing to take ownership of.
            return;
        };
        let _ = table.install_heir(heir.owner, Some(data));
    }

    /// Discharges the ETS obligations recorded since the last drain: every table a dead process
    /// owned is dropped, or handed to its live heir with the oracle's
    /// `{'ETS-TRANSFER', Tid, FromPid, Data}` message.
    ///
    /// The set of tables to act on is the `beam_bifs::TableRegistry::handles_owned_by` roster
    /// (which covers a table created **without** `named_table`, whose handle is the registry key
    /// and is reachable from no public term), unioned with the handles the index recorded at
    /// `ets:new/2` time and deduplicated in ascending handle order. The index contributes one
    /// thing the roster cannot: the **registered name** of a `named_table`, needed to release the
    /// name registration when the table is dropped.
    ///
    /// Must be called at a point where no ETS borrow from the runtime's own call path can be
    /// live (a scheduler-pass round boundary). The ETS registry is taken with `try_borrow_mut`:
    /// a still-live borrow defers the deaths to the next drain — never a panic, never a dropped
    /// obligation.
    ///
    /// Returns the number of transfer messages delivered. Idempotent and cheap with nothing
    /// pending.
    ///
    /// # Errors
    ///
    /// Propagates a heap exhaustion while building a transfer message (the same documented
    /// `error::system_limit`) and a delivery failure from the process table.
    pub(crate) fn drain_ets_owner_exits(
        &mut self,
        table: &mut ProcessTable,
        scheduler: &mut Scheduler,
    ) -> Result<usize> {
        let deaths = {
            let mut index = self.ets_owners.borrow_mut();
            std::mem::take(&mut index.deaths)
        };
        if deaths.is_empty() {
            return Ok(0);
        }
        let Ok(mut registry) = self.tables.try_borrow_mut() else {
            // The registry is held by an in-flight BIF call (a death reached the finalizer from
            // inside it). Put the deaths back, in order, in front of anything recorded since —
            // and never drop them.
            let mut index = self.ets_owners.borrow_mut();
            let mut deferred = deaths;
            deferred.append(&mut index.deaths);
            index.deaths = deferred;
            return Ok(0);
        };
        // `(heir pid, gift data, the handle's registered name, the handle, dead owner term)`
        // per pending transfer. `name` is `Some` exactly for a `named_table` (the only kind the
        // index records), and is what decides the tid: the name atom for a named table, the
        // handle's ref thing for an unnamed one (see [`tid_ref_thing`]).
        let mut transfers: Vec<(ProcessId, Eterm, Option<Atom>, u64, Eterm)> = Vec::new();
        for dead in deaths {
            // The registered names this owner's `named_table`s were created under, keyed by
            // handle. Taken (not just read) so the index holds nothing for a pid that has died.
            let recorded: HashMap<u64, Atom> = {
                let mut index = self.ets_owners.borrow_mut();
                index
                    .tables
                    .remove(&owner_key(dead))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|owned| (owned.handle, owned.name))
                    .collect()
            };
            let dead_owner = OwnerId::new(owner_key(dead));
            let from = dead.to_eterm();
            // The roster is the authority on **which** tables the dead pid owned — including an
            // *unnamed* table, which the index never held. The recorded handles are folded in
            // (deduplicated, ascending) so the index's view can never lose an obligation the
            // roster happened to omit; a handle the roster does not list is `NotOwner` at
            // `owner_died` and is skipped there.
            let mut handles = registry.handles_owned_by(TableRegistry::owner_of(dead));
            handles.extend(recorded.keys().copied());
            handles.sort_unstable();
            handles.dedup();
            for handle in handles {
                let name = recorded.get(&handle).copied();
                // The owner-death transition is `beam_ets`'s (it owns the heir/self-heir/dead-
                // heir rules); the runtime only acts on the obligation it returns. A handle
                // whose table was already deleted (`ets:delete/1`) is a stale index entry.
                let outcome = match registry.get_mut(handle) {
                    Some(entry) => entry.owner_died(dead_owner, |heir| owner_is_alive(table, heir)),
                    None => continue,
                };
                match outcome {
                    OwnerDeath::NotOwner => {}
                    OwnerDeath::Deleted => {
                        registry.remove(handle);
                        // Only a `named_table` registered a name (an unnamed table still records
                        // its name field but takes part in no registry — `beam_bifs::ets::new_2`).
                        if let Some(name) = name {
                            registry.remove_name(name);
                        }
                    }
                    OwnerDeath::Transferred(heir) => {
                        // An arity-2 `{heir, Pid}` inherits in silence (the oracle sends no
                        // message); only a `{heir, Pid, Data}` carries gift data and a message.
                        if let Some(data) = heir.data {
                            if let Ok(to) =
                                ProcessId::from_eterm(Eterm::from_raw(heir.owner.raw() as usize))
                            {
                                transfers.push((to, data, name, handle, from));
                            }
                        }
                    }
                }
            }
        }
        drop(registry);
        if transfers.is_empty() {
            return Ok(0);
        }
        // The message is built on the pooled BIF heap (returned below) and copied into the
        // receiver's mailbox by the delivery, exactly like the port half's messages.
        // The tag is the **bare** atom name: `intern` takes the atom's characters, not an
        // Erlang source literal, so the quoting is the printer's business (`'ETS-TRANSFER'`
        // here would intern an atom whose name literally contains quotes).
        let tag = self.intern("ETS-TRANSFER");
        let mut heap = self.take_bif_heap()?;
        let mut delivered = 0usize;
        for (to, data, name, handle, from) in transfers {
            // A terminated heir's slot is reclaimed and its serial bumped, so a stale pid must
            // be checked before delivery (the port half's model).
            if !table.contains(to) {
                continue;
            }
            let tid = match name {
                Some(name) => make_atom(name),
                None => tid_ref_thing(&mut heap, handle)?,
            };
            let message = heap.tuple(&[tag, tid, from, data])?;
            table.deliver_simple(to, from, message)?;
            delivered += 1;
        }
        self.put_bif_heap(heap);
        if delivered > 0 {
            // The heir is blocked in a `receive`: enqueue it through the normal resume-on-send
            // path so the pass that discharged the obligation also runs it.
            scheduler.resume_woken(table);
        }
        Ok(delivered)
    }

    /// Sweeps the **dangling incoming port monitors** dead watchers left on the runtime's live
    /// port table (contract A.4).
    ///
    /// `erlang:monitor(port, Port)` records an incoming
    /// [`PortMonitor`](beam_process::PortMonitor) on the port (the runtime's authoritative
    /// `beam_process::PortTable`, see [`crate::port`]) so a port close can deliver
    /// `{'DOWN', Ref, port, Port, Reason}` to the watcher. When the **watcher** dies instead,
    /// that incoming entry outlives it: `beam_process`'s finalizer removes the matching
    /// incoming entry only when a `PortTable` is threaded into the termination
    /// ([`ProcessTable::terminate_with_ports`](beam_process::ProcessTable)), which this runtime
    /// does not do — it hands the port table to no termination path. The
    /// [`PortTable::monitors`](beam_process::PortTable::monitors) set would therefore keep the
    /// dead pid forever (and `erlang:port_info(Port, monitors)`-style introspection would
    /// report a watcher that cannot receive a `'DOWN'`).
    ///
    /// The exit observer therefore records each dead watcher's
    /// [`ProcessExit::port_monitors`](beam_process::ProcessExit::port_monitors) pairs
    /// ([`OwnerIndex::record_port_watches`]) and this drain removes exactly those entries —
    /// there is no by-watcher-pid lookup on [`PortTable`](beam_process::PortTable), so the
    /// `(port, reference)` pairs are the only handle. Only the pairs the table really holds are
    /// removed: a port that has already closed drained its own side, so an unallocated id is
    /// skipped rather than treated as an error (`demonitor` would answer `BadArg` for it).
    ///
    /// Must be called at a point where no borrow of the runtime's port registry from the call
    /// path can be live (the same scheduler-pass round boundary as
    /// [`Runtime::drain_ets_owner_exits`]). The registry is taken with `try_borrow_mut`: a
    /// still-live borrow defers the sweep to the next drain — never a panic, never a dropped
    /// obligation.
    ///
    /// Returns the number of entries removed. Idempotent and cheap with nothing pending.
    ///
    /// # Errors
    ///
    /// Returns [`BeamError::BadArg`](beam_common::BeamError::BadArg) only if a monitor survives
    /// on a port that `contains` said was allocated and then vanished — unreachable under the
    /// single-threaded runtime, so it is propagated rather than swallowed.
    pub(crate) fn drain_port_monitor_exits(&mut self) -> Result<usize> {
        let watches = {
            let mut index = self.ets_owners.borrow_mut();
            index.take_port_watches()
        };
        if watches.is_empty() {
            return Ok(0);
        }
        // The `Rc` is cloned **before** the borrow so the returned `RefMut` borrows the clone
        // rather than the runtime field — the same shape `crate::native::process::monitor` and
        // `crate::port::deliver_port_messages` use.
        let cell = Rc::clone(&self.ports);
        let Ok(mut registry) = cell.try_borrow_mut() else {
            // The port registry is held by an in-flight BIF call (a death reached the finalizer
            // from inside it). Put the watches back, in order, in front of anything recorded
            // since — and never drop them.
            let mut index = self.ets_owners.borrow_mut();
            let mut deferred = watches;
            deferred.append(&mut index.port_watches);
            index.port_watches = deferred;
            return Ok(0);
        };
        let table = registry.port_table_mut();
        let mut removed = 0usize;
        for (port, reference) in watches {
            if !table.contains(port) {
                // The port closed before this drain: its own side is already gone.
                continue;
            }
            if table.demonitor(port, reference)? {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recording is per `(owner, handle)`: a re-observation never duplicates an entry.
    #[test]
    fn recording_is_idempotent_per_handle() {
        let mut index = OwnerIndex::default();
        let owner = ProcessId::new(0, 5);
        let name = Atom::from_raw(7);
        index.record(owner, 3, name);
        index.record(owner, 3, name);
        assert_eq!(index.owner_count(), 1);
        assert_eq!(index.tables[&owner_key(owner)].len(), 1);
    }

    /// Two owners are keyed apart, and a death is recorded once per report.
    #[test]
    fn owners_are_keyed_apart_and_deaths_are_recorded() {
        let mut index = OwnerIndex::default();
        index.record(ProcessId::new(0, 1), 1, Atom::from_raw(1));
        index.record(ProcessId::new(0, 2), 2, Atom::from_raw(2));
        assert_eq!(index.owner_count(), 2);
        assert_eq!(index.pending_deaths(), 0);
    }

    /// `owner_key` round-trips through the pid immediate: the key a table read back yields is
    /// the key the index recorded.
    #[test]
    fn owner_key_round_trips_through_the_pid_immediate() {
        let pid = ProcessId::new(3, 9);
        let term = pid.to_eterm();
        assert_eq!(owner_key(pid), term.raw() as u64);
        assert_eq!(
            ProcessId::from_eterm(Eterm::from_raw(owner_key(pid) as usize)).expect("decodes"),
            pid
        );
    }

    /// The one non-obvious step of the [`tid_ref_thing`] substitution: a fresh [`RefAllocator`]
    /// draws the ids `0, 1, 2, …`, so `handle + 1` draws land **exactly** on `handle`. An
    /// off-by-one here would mint a tid that does not decode back to the table — the one
    /// outcome the substitution must never produce. (The end-to-end proof that a tid built this
    /// way really *resolves* is `tests/ets_owner.rs`'s unnamed-transfer flow, where the heir
    /// `ets:insert/2`s through the received tid.)
    #[test]
    fn the_tid_substitution_lands_on_the_handle_id() {
        for handle in [0u64, 1, 2, 7, 4096] {
            let mut refs = RefAllocator::new();
            let mut reference = refs.next();
            for _ in 0..handle {
                reference = refs.next();
            }
            assert_eq!(reference.id(), handle, "the substitution must land on {handle}");
        }
    }

    /// The observer's port-watch half records every `(port, reference)` pair a death drained,
    /// append-only (two deaths on the same port are two separate obligations), and a drain
    /// takes the whole backlog out — so a second drain has nothing to do.
    #[test]
    fn port_watches_are_recorded_append_only_and_taken_for_a_drain() {
        let port_a = PortId::new(4);
        let port_b = PortId::new(5);
        // `MonitorRef` has no public constructor: references come out of the id allocator
        // (the runtime's single ref space — see `crate::runtime::Runtime::refs_mut`).
        let mut refs = beam_process::RefAllocator::new();
        let (r1, r2, r3) = (refs.next(), refs.next(), refs.next());
        let mut index = OwnerIndex::default();
        assert_eq!(index.pending_port_watches(), 0);

        index.record_port_watches(&[
            PortWatch {
                reference: r1,
                port: port_a,
            },
            PortWatch {
                reference: r2,
                port: port_b,
            },
        ]);
        index.record_port_watches(&[PortWatch {
            reference: r3,
            port: port_a,
        }]);
        assert_eq!(index.pending_port_watches(), 3);

        let taken = index.take_port_watches();
        assert_eq!(taken, vec![(port_a, r1), (port_b, r2), (port_a, r3)]);
        assert_eq!(index.pending_port_watches(), 0);
        assert!(index.take_port_watches().is_empty());
    }
}