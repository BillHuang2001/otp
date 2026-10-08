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

//! Integration evidence for the port ownership / link / monitor surface.
//!
//! Covers the `PortTable` round-trips (`alloc_owned`/`owner`/`set_owner`/`clear_owner`,
//! link dedup/unlink, monitor/demonitor) and the one-call `ProcessTable::port_closed`
//! orchestrator a dying port uses: a monitor is answered by a `{'DOWN', Ref, port, Port,
//! Reason}` message, a **trapping** linked process receives `{'EXIT', Port, Reason}` and
//! survives, a **non-trapping** one terminates with the reason, and an unknown port is
//! rejected without any side effect.

use beam_common::BeamError;
use beam_process::prelude::*;
use beam_term::prelude::{
    make_atom, make_small, tuple_element, tuple_size, Atom, AtomTable, Eterm,
};

/// A pid with the given number and serial 0.
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

/// A table plus the raw indices of the well-known signal atoms a port message is built with.
fn table_with_atoms() -> (ProcessTable, Atom, Atom, Atom) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    let exit = atoms.exit();
    let down = atoms.down();
    let port = atoms.port();
    (ProcessTable::new(atoms), exit, down, port)
}

/// The payload of `pid`'s single queued message (panics unless exactly one is queued).
fn sole_payload(table: &ProcessTable, pid: ProcessId) -> Eterm {
    let mailbox = table.get(pid).expect("the recipient is live").mailbox();
    assert_eq!(mailbox.len(), 1, "exactly one message is queued");
    mailbox.peek().expect("one message is queued").payload()
}

#[test]
fn port_ownership_round_trips() {
    let mut ports = PortTable::new();
    let owner = pid(1);
    let other = pid(2);

    let id = ports.alloc_owned(owner).expect("room in the number space");
    assert_eq!(ports.owner(id), Some(owner));

    ports.set_owner(id, other).expect("the id is allocated");
    assert_eq!(ports.owner(id), Some(other));

    ports.clear_owner(id);
    assert_eq!(ports.owner(id), None, "clearing drops the owner");
    assert_eq!(ports.owner(PortId::new(9999)), None, "a never-allocated id");

    // `release` clears the owner too.
    ports.set_owner(id, owner).expect("the id is allocated");
    assert_eq!(ports.owner(id), Some(owner));
    ports.release(id);
    assert_eq!(ports.owner(id), None);
    assert!(!ports.contains(id));

    // Setting an owner on an unallocated id is badarg.
    assert!(matches!(
        ports.set_owner(PortId::new(9999), owner),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn port_links_are_recorded_deduplicated_and_removed() {
    let mut ports = PortTable::new();
    let id = ports.alloc().expect("room in the number space");
    let a = pid(10);
    let b = pid(11);

    assert_eq!(ports.links(id), &[] as &[ProcessId]);
    assert!(ports.link(id, a).expect("the id is allocated"));
    assert!(
        !ports.link(id, a).expect("the id is allocated"),
        "re-linking an already-linked pair is a no-op"
    );
    assert!(ports.link(id, b).expect("the id is allocated"));
    assert_eq!(ports.links(id), &[a, b]);

    assert!(ports.unlink(id, a).expect("the id is allocated"));
    assert!(!ports.unlink(id, a).expect("the id is allocated"));
    assert_eq!(ports.links(id), &[b]);

    // `link`/`unlink`/`monitor` on an unallocated id are all badarg.
    let unknown = PortId::new(9999);
    let reference = RefAllocator::new().next();
    assert!(matches!(ports.link(unknown, a), Err(BeamError::BadArg)));
    assert!(matches!(ports.unlink(unknown, a), Err(BeamError::BadArg)));
    assert!(matches!(
        ports.monitor(unknown, b, reference),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn a_monitor_on_a_port_yields_the_port_tagged_down_shape() {
    let (mut table, _exit, down, port_atom) = table_with_atoms();
    let owner = table.spawn_default().expect("owner");
    let watcher = table.spawn_default().expect("watcher");

    let mut ports = PortTable::new();
    let port = ports.alloc_owned(owner).expect("room in the number space");
    let reference = RefAllocator::new().next();
    ports
        .monitor(port, watcher, reference)
        .expect("the id is allocated");

    let reason = make_small(17);
    table
        .port_closed(&mut ports, port, reason)
        .expect("the port is allocated");

    // Closing frees the number and clears its monitors.
    assert!(!ports.contains(port));
    assert!(ports.monitors(port).is_empty());
    assert_eq!(ports.owner(port), None);

    let payload = sole_payload(&table, watcher);
    // SAFETY: `payload` is the live `{'DOWN', Ref, port, Port, Reason}` tuple the table
    // built on the watcher's heap; the five element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), make_atom(down));
        assert_eq!(tuple_element(payload, 2), make_atom(port_atom));
        assert_eq!(tuple_element(payload, 3), port.to_eterm());
        assert_eq!(tuple_element(payload, 4), reason);
    }
}

#[test]
fn a_port_close_signals_linked_processes() {
    let (mut table, exit, _down, _port_atom) = table_with_atoms();
    let owner = table.spawn_default().expect("owner");
    let doomed = table.spawn_default().expect("non-trapping peer");
    let survivor = table.spawn_default().expect("trapping peer");
    table.get_mut(survivor).expect("survivor").set_trap_exit(true);

    let mut ports = PortTable::new();
    let port = ports.alloc_owned(owner).expect("room in the number space");
    ports.link(port, doomed).expect("the id is allocated");
    ports.link(port, survivor).expect("the id is allocated");

    let reason = make_small(21);
    table
        .port_closed(&mut ports, port, reason)
        .expect("the port is allocated");

    assert!(!ports.contains(port), "the port is closed");
    assert!(
        !table.contains(doomed),
        "a non-trapping linked process terminates with the reason"
    );
    assert!(table.contains(survivor), "a trapping peer survives");

    let payload = sole_payload(&table, survivor);
    // SAFETY: `payload` is the live `{'EXIT', Port, Reason}` tuple the table built on the
    // survivor's heap; the three element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), make_atom(exit));
        assert_eq!(tuple_element(payload, 1), port.to_eterm());
        assert_eq!(tuple_element(payload, 2), reason);
    }
}

#[test]
fn port_closed_on_an_unknown_port_is_badarg() {
    let (mut table, _exit, _down, _port_atom) = table_with_atoms();
    let owner = table.spawn_default().expect("owner");
    let watcher = table.spawn_default().expect("watcher");

    let mut ports = PortTable::new();
    let port = ports.alloc_owned(owner).expect("room in the number space");
    let reference = RefAllocator::new().next();
    ports
        .monitor(port, watcher, reference)
        .expect("the id is allocated");

    // An unknown port is rejected up front: nothing is signalled, nothing changes.
    let before = table.len();
    assert!(matches!(
        table.port_closed(&mut ports, PortId::new(9999), make_small(1)),
        Err(BeamError::BadArg)
    ));
    assert_eq!(table.len(), before, "no process was terminated");
    assert!(ports.contains(port), "the live port is untouched");
    assert_eq!(ports.monitors(port), &[PortMonitor { reference, watcher }]);
    assert_eq!(
        table.get(watcher).expect("watcher").mailbox().len(),
        0,
        "the watcher received no message"
    );
}
