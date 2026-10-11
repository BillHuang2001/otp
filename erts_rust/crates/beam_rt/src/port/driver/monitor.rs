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

//! The per-port **driver monitor tree** (contract **A.4**): the state behind the three
//! [`beam_abi::DriverHost`] monitor hooks — `driver_monitor_process`,
//! `driver_demonitor_process` and `driver_get_monitored_process`.
//!
//! # What a driver monitor is
//!
//! A C driver may watch Erlang processes: `driver_monitor_process(port, pid, &monitor)` fills the
//! caller's `ErlDrvMonitor` holder (a **32-byte** struct whose first 24 bytes are the emulator's
//! internal *reference thing* — `size_of::<Eterm>() * 3`, `io.c:6996-6999`), and the driver may
//! later ask the runtime for the monitored pid (`driver_get_monitored_process`) or cancel the watch
//! (`driver_demonitor_process`). The C keeps the monitor in a per-port list keyed on that reference
//! thing (`driver_compare_monitors` is a plain `memcmp` of those 24 bytes, `io.c:7142-7147`) and
//! **fires** `ErlDrvEntry::process_exit` when a monitored process dies, removing the monitor as it
//! fires.
//!
//! This module is that list, reduced to the one thing a `#![deny(unsafe_code)]` host can key on: the
//! opaque 24-byte identity. `beam_abi` exposes the two safe ends of the raw `ErlDrvMonitor*` the
//! hooks receive and hand back — [`monitor_identity`](beam_abi::monitor_identity) (read the bytes
//! out) and [`monitor_from_identity`](beam_abi::monitor_from_identity) (place a chosen identity into
//! a holder to return) — so nothing here ever dereferences a pointer.
//!
//! # What this module does *not* do
//!
//! It does **not** itself fire `process_exit`, and it does **not** observe a process dying: the hook
//! seam gives a per-port host no route to the live process table (it holds only its owning pid). The
//! runtime observes process exits elsewhere and marks the tree through
//! [`DriverMonitors::note_process_exit`]; the port lifecycle then fires one callback per entry
//! [`DriverMonitors::take_pending`] yields, at the per-round drive point
//! ([`DriverPortAdapter::drive_async`](super::DriverPortAdapter::drive_async)). A tree that is never
//! driven simply holds monitors that never fire, exactly like a port whose driver declared no
//! `process_exit` slot.

use std::cell::RefCell;
use std::rc::Rc;

use beam_abi::{monitor_from_identity, ErlDrvMonitor};
use beam_process::ProcessId;

/// One monitor a driver holds on a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    /// The opaque 24-byte identity handed to the driver — the exact bytes the C's
    /// `driver_compare_monitors` compares (`io.c:7142`), and the only thing a later
    /// `driver_demonitor_process`/`driver_get_monitored_process` carries to find this entry again.
    identity: [u8; 24],
    /// The monitored process, as this runtime's typed handle.
    pid: ProcessId,
    /// Whether the monitored process has exited and this monitor awaits firing through
    /// `ErlDrvEntry::process_exit` (see [`DriverMonitors::note_process_exit`]).
    pending: bool,
}

/// The mutable tree state shared through the [`RefCell`].
#[derive(Debug, Default)]
struct Inner {
    /// The monitors this port holds, in installation order.
    entries: Vec<Entry>,
    /// The per-port identity counter: every [`DriverMonitors::install`] bumps it, so a fresh
    /// identity is minted for every monitor.
    next: u64,
}

/// The per-port tree of monitors a driver holds on processes (`driver_monitor_process`).
///
/// One handle per port, shared by [`Rc`] between the host (whose hooks install, look up and cancel
/// monitors from inside a driver callback) and the runtime (which seizes the handle before the host
/// is boxed into the port lifecycle, and later drains it to fire `process_exit`) — the same sharing
/// pattern as [`TermQueue`](super::TermQueue) and
/// [`DriverAtoms`](super::DriverAtoms).
///
/// The identity is minted here rather than taken from the monitored pid: the C hands out a fresh
/// reference for every watch, and the driver treats those bytes as opaque. Two ports must therefore
/// never mint the same identity — a per-port counter alone would, since every port starts at zero —
/// so the owning process's number is mixed into the upper half of the identity.
#[derive(Debug)]
pub struct DriverMonitors {
    /// The owning port's process number, mixed into every minted identity so two ports do not
    /// collide.
    owner: u64,
    /// The shared tree state.
    inner: RefCell<Inner>,
}

impl DriverMonitors {
    /// A new, empty tree for the port owned by `owner`, as a handle the runtime can share.
    pub fn new(owner: ProcessId) -> Rc<DriverMonitors> {
        Rc::new(DriverMonitors {
            owner: owner.number(),
            inner: RefCell::new(Inner::default()),
        })
    }

    /// Installs a monitor on `pid` and returns the holder to hand back to the driver
    /// (`driver_monitor_process`, `io.c:6998`).
    ///
    /// The identity is fresh on every call: the per-port counter is bumped and mixed with the
    /// owning process's number (see the type docs), and the entry is appended in installation order.
    ///
    /// # Panics
    ///
    /// Never in practice: the tree is only borrowed for the length of one of these methods, and no
    /// method re-enters another (a driver callback never holds the borrow across a call), so the
    /// `borrow_mut` here always succeeds. The same rule
    /// [`TermQueue::take`](super::TermQueue::take) relies on.
    pub fn install(&self, pid: ProcessId) -> ErlDrvMonitor {
        let mut inner = self.inner.borrow_mut();
        inner.next = inner.next.wrapping_add(1);
        let identity = self.mint_identity(inner.next);
        inner.entries.push(Entry {
            identity,
            pid,
            pending: false,
        });
        monitor_from_identity(identity)
    }

    /// The process monitored by `identity`, or `None` when this port holds no such monitor.
    ///
    /// A nested borrow (impossible in practice — see [`DriverMonitors::install`]) is answered `None`,
    /// the same "no such monitor" the driver would get for an unknown identity, never a panic.
    pub fn find(&self, identity: [u8; 24]) -> Option<ProcessId> {
        let inner = self.inner.try_borrow().ok()?;
        inner
            .entries
            .iter()
            .find(|entry| entry.identity == identity)
            .map(|entry| entry.pid)
    }

    /// Removes the monitor `identity` (`driver_demonitor_process`); `true` when one was removed,
    /// `false` when this port holds no such monitor (the C's "it no longer exists").
    pub fn remove(&self, identity: [u8; 24]) -> bool {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return false;
        };
        let Some(index) = inner
            .entries
            .iter()
            .position(|entry| entry.identity == identity)
        else {
            return false;
        };
        inner.entries.remove(index);
        true
    }

    /// Whether any monitor this port holds is awaiting its `process_exit` firing.
    pub fn has_pending(&self) -> bool {
        match self.inner.try_borrow() {
            Ok(inner) => inner.entries.iter().any(|entry| entry.pending),
            Err(_) => false,
        }
    }

    /// Drains the monitors awaiting firing, returning their identities in installation order.
    ///
    /// A drained monitor is **removed** from the tree, mirroring the C, which unlinks a monitor as
    /// it fires `process_exit` — so a later `driver_demonitor_process` on that monitor reports the
    /// C's "no longer exists", and the identity is never yielded twice.
    pub fn take_pending(&self) -> Vec<[u8; 24]> {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return Vec::new();
        };
        let mut taken = Vec::new();
        let mut index = 0;
        while index < inner.entries.len() {
            if inner.entries[index].pending {
                taken.push(inner.entries.remove(index).identity);
            } else {
                index += 1;
            }
        }
        taken
    }

    /// Marks every monitor this port holds on `pid` as awaiting firing, answering how many entries
    /// were newly marked.
    ///
    /// The runtime calls this when it observes `pid` exit; it is idempotent for a pid already marked,
    /// so the count is the number of *newly* pending monitors (what a caller would fire). The entry
    /// itself stays until [`DriverMonitors::take_pending`] removes it, so
    /// [`DriverMonitors::monitors`] still answers `true` for `pid` until the monitor is fired or
    /// cancelled.
    pub fn note_process_exit(&self, pid: ProcessId) -> usize {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return 0;
        };
        let mut marked = 0;
        for entry in inner.entries.iter_mut() {
            if entry.pid == pid && !entry.pending {
                entry.pending = true;
                marked += 1;
            }
        }
        marked
    }

    /// The distinct processes this port currently watches, in installation order.
    ///
    /// The runtime's exit sweep asks this for each port's watch set and tests every name it returns
    /// against the live process table, so a dead monitored process can be marked through
    /// [`DriverMonitors::note_process_exit`] without this module ever seeing that table. A pid
    /// watched by several monitors is reported **once** (the number of monitors is not what the
    /// sweep needs), and a nested borrow (impossible in practice — see
    /// [`DriverMonitors::install`]) is answered empty, the same "no monitors" a caller reads from
    /// [`DriverMonitors::has_pending`], never a panic.
    pub fn monitored_pids(&self) -> Vec<ProcessId> {
        let Ok(inner) = self.inner.try_borrow() else {
            return Vec::new();
        };
        let mut pids: Vec<ProcessId> = Vec::new();
        for entry in inner.entries.iter() {
            if !pids.contains(&entry.pid) {
                pids.push(entry.pid);
            }
        }
        pids
    }

    /// Whether this port holds any monitor on `pid` — the runtime's "does this port still watch that
    /// process" test.
    pub fn monitors(&self, pid: ProcessId) -> bool {
        match self.inner.try_borrow() {
            Ok(inner) => inner.entries.iter().any(|entry| entry.pid == pid),
            Err(_) => false,
        }
    }

    /// Drops every monitor — the port (and the driver that held them) is going away.
    ///
    /// The identity counter is deliberately **not** reset: a cleared tree never re-mints an identity
    /// it has already handed out.
    pub fn clear(&self) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.entries.clear();
        }
    }

    /// Mints the fresh identity for counter value `serial`: the counter in the low 8 bytes and the
    /// owning process's number in the next 8, the remaining 8 bytes zero.
    fn mint_identity(&self, serial: u64) -> [u8; 24] {
        let mut identity = [0u8; 24];
        identity[..8].copy_from_slice(&serial.to_le_bytes());
        identity[8..16].copy_from_slice(&self.owner.to_le_bytes());
        identity
    }
}
