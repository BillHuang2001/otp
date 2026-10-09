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

//! **ETS owner-death reclamation** (the `beam_process` `ProcessExitObserver` host).
//!
//! An ETS table belongs to the process that created it, so the runtime must release it the
//! moment that process dies — the table disappears, or it is handed to its installed heir
//! (`beam_ets::Table::owner_died`, contract A.1/`erl_db.c`). `beam_process` cannot know what
//! the ETS registry allocated on a pid's behalf, so it exposes the
//! [`ProcessExitObserver`](beam_process::ProcessExitObserver) seam: the runtime installs an
//! observer on its [`ProcessTable`] and is called **exactly once per death**, from inside the
//! finalizer, before the dying heap is released.
//!
//! # Why the observer only *records*
//!
//! The finalizer can run while a `RefCell` the runtime owns is already borrowed — a BIF call
//! holds the shared ETS [`TableRegistry`](beam_bifs::TableRegistry) for the whole of its call,
//! and a death inside that call would be reached with the cell taken. An observer that reached
//! for the registry there would either panic or silently lose the obligation. So the observer
//! touches **nothing but its own** [`OwnerIndex`] cell (a cell no BIF ever borrows): it pushes
//! the dead pid and returns. [`Runtime::drain_ets_owner_exits`] then discharges the recorded
//! obligations at a point where no ETS borrow is live — a scheduler-pass round — using
//! `try_borrow_mut` and **deferring** (never dropping, never panicking) when the registry is
//! still held.
//!
//! # What is reclaimed, and the honest gap
//!
//! `beam_bifs::TableRegistry` exposes no public owner→handle roster (`all_tids` is
//! `pub(super)`) and no handle iterator, and an *unnamed* table's handle is only reachable from
//! its on-heap ref thing through `unsafe`/crate-private decoders. This crate therefore indexes
//! the tables it can prove: a **`named_table`**, whose handle is resolvable through the public
//! `TableRegistry::lookup_name` surface at `ets:new/2` time (the creation is observed in
//! [`RuntimeContext::call_bif`](crate::context::RuntimeContext::call_bif), on the same route
//! every `ets:new/2` takes). A dead owner's named tables are dropped (and their name
//! registration released), or handed to a live heir with the oracle's
//! `{'ETS-TRANSFER', Tid, FromPid, Data}` message — carrying the table's **name atom** as the
//! tid, exactly as the oracle does for a `named_table`.
//!
//! **Gap (an honest handoff, never a wrong result).** A table created *without* `named_table`
//! is not indexed, so a dead owner's unnamed table is not reclaimed here (it leaks, as it did
//! before this module existed) and no fabricated tid is ever produced. Closing that needs a
//! public owner/handle roster (or a creation hook reporting "this call created handle `H` for
//! pid `P`") on `beam_bifs::TableRegistry` — reported as a handoff.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use beam_common::Result;
use beam_ets::{OwnerDeath, OwnerId};
use beam_process::{
    ProcessExit, ProcessExitObserver, ProcessId, ProcessTable, Scheduler,
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

/// The owner→named-table index plus the deaths waiting to be discharged.
///
/// Shared through an [`Rc`]`<`[`RefCell`]`>` between the [`Runtime`] (which records creations
/// and drains) and the [`EtsExitObserver`] (which records deaths). No BIF ever borrows this
/// cell, so the observer can always take it — that is what makes the observer safe to run from
/// inside the finalizer while an ETS borrow is live.
#[derive(Default)]
pub(crate) struct OwnerIndex {
    /// The `named_table`s each live owner created, keyed by [`owner_key`].
    tables: HashMap<u64, Vec<OwnedTable>>,
    /// The pids observed dead, in death order, waiting for a drain.
    deaths: Vec<ProcessId>,
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

    /// The number of owners with at least one indexed table (test/diagnostic).
    pub(crate) fn owner_count(&self) -> usize {
        self.tables.len()
    }

    /// The number of deaths waiting for a drain (test/diagnostic).
    pub(crate) fn pending_deaths(&self) -> usize {
        self.deaths.len()
    }
}

/// The `ProcessExitObserver` host: records the dead pid, and nothing else.
///
/// The `reason` is deliberately **not** retained (it may live in the dying heap, and no part
/// of the ETS obligation needs it), so there is nothing to copy.
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
        // Only the pid is retained: it is an immediate (heap-independent), and it is the whole
        // obligation — the affected tables are looked up at drain time.
        self.index.borrow_mut().deaths.push(exit.pid());
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

impl Runtime {
    /// Discharges the ETS obligations recorded since the last drain: every indexed table owned
    /// by a process that has since died is dropped, or handed to its live heir with the
    /// oracle's `{'ETS-TRANSFER', Tid, FromPid, Data}` message.
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
        // `(heir pid, gift data, table name, dead owner term)` per pending transfer.
        let mut transfers: Vec<(ProcessId, Eterm, Atom, Eterm)> = Vec::new();
        for dead in deaths {
            let owned = {
                let mut index = self.ets_owners.borrow_mut();
                index.tables.remove(&owner_key(dead)).unwrap_or_default()
            };
            if owned.is_empty() {
                continue;
            }
            let dead_owner = OwnerId::new(owner_key(dead));
            let from = dead.to_eterm();
            for record in owned {
                // The owner-death transition is `beam_ets`'s (it owns the heir/self-heir/dead-
                // heir rules); the runtime only acts on the obligation it returns. A handle
                // whose table was already deleted (`ets:delete/1`) is a stale index entry.
                let outcome = match registry.get_mut(record.handle) {
                    Some(entry) => entry.owner_died(dead_owner, |heir| owner_is_alive(table, heir)),
                    None => continue,
                };
                match outcome {
                    OwnerDeath::NotOwner => {}
                    OwnerDeath::Deleted => {
                        registry.remove(record.handle);
                        registry.remove_name(record.name);
                    }
                    OwnerDeath::Transferred(heir) => {
                        // An arity-2 `{heir, Pid}` inherits in silence (the oracle sends no
                        // message); only a `{heir, Pid, Data}` carries gift data and a message.
                        if let Some(data) = heir.data {
                            if let Ok(to) =
                                ProcessId::from_eterm(Eterm::from_raw(heir.owner.raw() as usize))
                            {
                                transfers.push((to, data, record.name, from));
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
        let tag = self.intern("'ETS-TRANSFER'");
        let mut heap = self.take_bif_heap()?;
        let mut delivered = 0usize;
        for (to, data, name, from) in transfers {
            // A terminated heir's slot is reclaimed and its serial bumped, so a stale pid must
            // be checked before delivery (the port half's model).
            if !table.contains(to) {
                continue;
            }
            let message = heap.tuple(&[tag, make_atom(name), from, data])?;
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
}
