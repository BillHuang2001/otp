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

//! The port-id table and the port **ownership / link / monitor** surface.
//!
//! [`PortTable`] is the number table the legacy C keeps alongside the pid table
//! (`erl_ptab.{c,h}`): it hands out the 60-bit port immediates
//! ([`PortId`](crate::pid::PortId)) that `beam_io`/`beam_rt` build their ports on. Port
//! *behaviour* — the I/O driver behind a port, its scheduling and its read path — is
//! `beam_io`'s; this module owns only the identity plus the relationships a dying port must
//! resolve: its **owner**, the processes **linked** to it and the **monitors** placed on it.
//!
//! A monitor on a port is one-directional just like a process monitor: the watcher keeps an
//! outgoing monitor and the port keeps the incoming [`PortMonitor`]. When the port closes,
//! [`crate::ptab::ProcessTable::port_closed`] drains these through [`PortTable::close`] and
//! delivers `{'DOWN', Ref, port, Port, Reason}` to every watcher and an
//! `{'EXIT', Port, Reason}` signal to every linked process.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_ptab.{c,h}` (the port table)
//! and `erl_monitor_link.c` (port links/monitors).

use std::collections::HashMap;

use beam_common::Result;
use beam_term::prelude::PORT_DATA_SIZE;

use crate::error;
use crate::monitor::MonitorRef;
use crate::pid::{PortId, ProcessId};

/// A monitor placed on a port: the watching process and the reference its `{'DOWN'}` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortMonitor {
    /// The reference the `{'DOWN', …}` message will carry.
    pub reference: MonitorRef,
    /// The watching process.
    pub watcher: ProcessId,
}

/// What a port releases when it closes: its owner, linked processes and monitors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortClose {
    /// The port's owner, when it had one.
    pub owner: Option<ProcessId>,
    /// The processes linked to the port.
    pub links: Vec<ProcessId>,
    /// The monitors placed on the port.
    pub monitors: Vec<PortMonitor>,
}

/// The port-id table plus the port ownership, link and monitor surface.
///
/// The number table itself is unchanged from the original minimal allocator (`slots` +
/// `free`); the three `HashMap`s record the relationships a closing port must resolve (see
/// [`crate::ptab::ProcessTable::port_closed`]). An unallocated number carries no
/// relationships, so a lookup for one yields `None`/an empty slice.
pub struct PortTable {
    /// Number → whether the slot is in use.
    slots: Vec<bool>,
    /// Free slot numbers available for reuse.
    free: Vec<u64>,
    /// Number → the process that owns the port.
    owners: HashMap<u64, ProcessId>,
    /// Number → the processes linked to the port.
    links: HashMap<u64, Vec<ProcessId>>,
    /// Number → the monitors placed on the port.
    monitors: HashMap<u64, Vec<PortMonitor>>,
}

impl PortTable {
    /// An empty port table.
    pub fn new() -> PortTable {
        PortTable {
            slots: Vec::new(),
            free: Vec::new(),
            owners: HashMap::new(),
            links: HashMap::new(),
            monitors: HashMap::new(),
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

    /// Allocates a fresh port id owned by `owner`.
    ///
    /// # Errors
    ///
    /// Propagates [`PortTable::alloc`]'s `BeamError::SystemLimit`.
    pub fn alloc_owned(&mut self, owner: ProcessId) -> Result<PortId> {
        let id = self.alloc()?;
        self.owners.insert(id.number(), owner);
        Ok(id)
    }

    /// Frees `number`'s slot, returning its number to the free list (a second call for the
    /// same, already-free number changes nothing).
    fn free_number(&mut self, number: u64) {
        if let Some(slot) = self.slots.get_mut(number as usize) {
            if *slot {
                *slot = false;
                self.free.push(number);
            }
        }
    }

    /// Releases `id`, returning its number to the free list and dropping every relationship
    /// (owner, links, monitors) it carried.
    pub fn release(&mut self, id: PortId) {
        let number = id.number();
        self.free_number(number);
        self.owners.remove(&number);
        self.links.remove(&number);
        self.monitors.remove(&number);
    }

    /// Closes `id`: frees its number like [`PortTable::release`] and returns — and clears —
    /// its owner, links and monitors, so the caller can resolve them.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not allocated.
    pub fn close(&mut self, id: PortId) -> Result<PortClose> {
        if !self.contains(id) {
            return Err(error::bad_pid());
        }
        let number = id.number();
        self.free_number(number);
        Ok(PortClose {
            owner: self.owners.remove(&number),
            links: self.links.remove(&number).unwrap_or_default(),
            monitors: self.monitors.remove(&number).unwrap_or_default(),
        })
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

    /// The owner of `id`, if it has one (and is allocated).
    pub fn owner(&self, id: PortId) -> Option<ProcessId> {
        self.owners.get(&id.number()).copied()
    }

    /// Installs `owner` as the owner of `id`, replacing any previous owner.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not allocated.
    pub fn set_owner(&mut self, id: PortId, owner: ProcessId) -> Result<()> {
        if !self.contains(id) {
            return Err(error::bad_pid());
        }
        self.owners.insert(id.number(), owner);
        Ok(())
    }

    /// Clears the owner of `id` (a no-op when it had none).
    pub fn clear_owner(&mut self, id: PortId) {
        self.owners.remove(&id.number());
    }

    /// Links `pid` to `id`, reporting whether the link was newly added.
    ///
    /// Like `Links::add`, re-linking an already-linked pair is a no-op that returns `false`.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not allocated.
    pub fn link(&mut self, id: PortId, pid: ProcessId) -> Result<bool> {
        if !self.contains(id) {
            return Err(error::bad_pid());
        }
        let links = self.links.entry(id.number()).or_default();
        if links.contains(&pid) {
            return Ok(false);
        }
        links.push(pid);
        Ok(true)
    }

    /// Unlinks `pid` from `id`, reporting whether the link was present.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not allocated.
    pub fn unlink(&mut self, id: PortId, pid: ProcessId) -> Result<bool> {
        if !self.contains(id) {
            return Err(error::bad_pid());
        }
        let Some(links) = self.links.get_mut(&id.number()) else {
            return Ok(false);
        };
        match links.iter().position(|entry| *entry == pid) {
            Some(index) => {
                links.remove(index);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// The processes linked to `id`, or an empty slice when it has none (or is unallocated).
    pub fn links(&self, id: PortId) -> &[ProcessId] {
        self.links
            .get(&id.number())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Places a monitor from `watcher` (carrying `reference`) on `id`.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not allocated.
    pub fn monitor(&mut self, id: PortId, watcher: ProcessId, reference: MonitorRef) -> Result<()> {
        if !self.contains(id) {
            return Err(error::bad_pid());
        }
        self.monitors
            .entry(id.number())
            .or_default()
            .push(PortMonitor { reference, watcher });
        Ok(())
    }

    /// Removes the monitor `reference` from `id`, reporting whether it was present.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `id` is not allocated.
    pub fn demonitor(&mut self, id: PortId, reference: MonitorRef) -> Result<bool> {
        if !self.contains(id) {
            return Err(error::bad_pid());
        }
        let Some(monitors) = self.monitors.get_mut(&id.number()) else {
            return Ok(false);
        };
        match monitors
            .iter()
            .position(|entry| entry.reference == reference)
        {
            Some(index) => {
                monitors.remove(index);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// The monitors placed on `id`, or an empty slice when it has none (or is unallocated).
    pub fn monitors(&self, id: PortId) -> &[PortMonitor] {
        self.monitors
            .get(&id.number())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for port allocation/reuse and the ownership/link/monitor round-trips,
    //! including that `release`/`close` drop the relationship maps. The propagation of a
    //! closing port to a linked/monitoring *process* is covered by `tests/ports.rs`.

    use beam_common::BeamError;

    use super::*;
    use crate::monitor::RefAllocator;

    /// A pid with the given number and serial 0.
    fn pid(number: u64) -> ProcessId {
        ProcessId::new(number, 0)
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

    #[test]
    fn owner_set_clear_release_and_unknown_ids() {
        let mut ports = PortTable::new();
        let owner = pid(1);
        let other = pid(2);

        let id = ports.alloc_owned(owner).expect("room");
        assert_eq!(ports.owner(id), Some(owner));
        assert_eq!(ports.owner(PortId::new(9999)), None, "never-allocated id");

        ports.set_owner(id, other).expect("the id is allocated");
        assert_eq!(ports.owner(id), Some(other));
        ports.clear_owner(id);
        assert_eq!(ports.owner(id), None, "clear drops the owner");

        // `release` also clears ownership.
        ports.set_owner(id, owner).expect("the id is allocated");
        ports.release(id);
        assert_eq!(ports.owner(id), None);
        assert!(!ports.contains(id));

        // An unallocated id rejects every relationship mutation with badarg.
        assert!(matches!(
            ports.set_owner(PortId::new(9999), owner),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn links_are_recorded_deduplicated_and_removed() {
        let mut ports = PortTable::new();
        let id = ports.alloc().expect("room");
        let a = pid(5);
        let b = pid(6);

        assert_eq!(ports.links(id), &[] as &[ProcessId]);
        assert!(ports.link(id, a).expect("allocated"));
        assert!(!ports.link(id, a).expect("allocated"), "deduped");
        assert!(ports.link(id, b).expect("allocated"));
        assert_eq!(ports.links(id), &[a, b]);

        assert!(ports.unlink(id, a).expect("allocated"));
        assert!(!ports.unlink(id, a).expect("allocated"));
        assert_eq!(ports.links(id), &[b]);

        // Releasing drops the link set.
        ports.release(id);
        assert_eq!(ports.links(id), &[] as &[ProcessId]);
    }

    #[test]
    fn monitors_are_recorded_and_removed_by_reference() {
        let mut ports = PortTable::new();
        let id = ports.alloc().expect("room");
        let mut refs = RefAllocator::new();
        let r1 = refs.next();
        let r2 = refs.next();
        let watcher = pid(7);

        assert_eq!(ports.monitors(id), &[] as &[PortMonitor]);
        ports.monitor(id, watcher, r1).expect("allocated");
        ports.monitor(id, watcher, r2).expect("allocated");
        assert_eq!(
            ports.monitors(id),
            &[
                PortMonitor {
                    reference: r1,
                    watcher
                },
                PortMonitor {
                    reference: r2,
                    watcher
                }
            ]
        );

        assert!(ports.demonitor(id, r1).expect("allocated"));
        assert!(!ports.demonitor(id, r1).expect("allocated"));
        assert_eq!(
            ports.monitors(id),
            &[PortMonitor {
                reference: r2,
                watcher
            }]
        );
    }

    #[test]
    fn close_frees_and_drains_every_relationship() {
        let mut ports = PortTable::new();
        let owner = pid(1);
        let linked = pid(2);
        let watcher = pid(3);
        let reference = RefAllocator::new().next();

        let id = ports.alloc_owned(owner).expect("room");
        ports.link(id, linked).expect("allocated");
        ports.monitor(id, watcher, reference).expect("allocated");

        let closed = ports.close(id).expect("the id is allocated");
        assert_eq!(closed.owner, Some(owner));
        assert_eq!(closed.links, vec![linked]);
        assert_eq!(
            closed.monitors,
            vec![PortMonitor {
                reference,
                watcher
            }]
        );

        // The number is freed and every relationship cleared.
        assert!(!ports.contains(id));
        assert_eq!(ports.owner(id), None);
        assert_eq!(ports.links(id), &[] as &[ProcessId]);
        assert_eq!(ports.monitors(id), &[] as &[PortMonitor]);

        // Closing an already-closed/unknown port is badarg.
        assert!(matches!(ports.close(id), Err(BeamError::BadArg)));
        assert!(matches!(
            ports.close(PortId::new(9999)),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn relationship_mutations_reject_an_unallocated_id() {
        let mut ports = PortTable::new();
        let unknown = PortId::new(9999);
        let a = pid(1);
        let reference = RefAllocator::new().next();

        assert!(matches!(ports.link(unknown, a), Err(BeamError::BadArg)));
        assert!(matches!(ports.unlink(unknown, a), Err(BeamError::BadArg)));
        assert!(matches!(
            ports.monitor(unknown, a, reference),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            ports.demonitor(unknown, reference),
            Err(BeamError::BadArg)
        ));
    }
}
