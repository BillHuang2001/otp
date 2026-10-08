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

//! Integration evidence for distributed (remote-node) links and monitors.
//!
//! Covers the `beam_process` remote-relationship capability: an **external pid** term links
//! and monitors through the same table entry points as a local pid, is recorded separately
//! (it never enters a `Process`'s own link/monitor sets), and every relationship change is
//! handed to the distribution seam ([`DistSignalRouter`]) as the exact control signal the
//! wire needs. A whole node going away (`node_down`) resolves the recorded monitors and links
//! exactly as the signals would have — a `{'DOWN', …}` on each watcher and a trapped or fatal
//! `{'EXIT', …}` on each linked peer — and then purges the records.
//!
//! The distribution seam itself is **not** exercised as a transport here: a `RecordingRouter`
//! records what a real `beam_dist`-backed router would have serialised. That is the handoff
//! documented on `src/remote.rs`.

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_process::prelude::*;
use beam_term::prelude::{
    boxed_val, external_pid_size_words, is_external_pid, make_atom, make_small, signed_value,
    tuple_element, tuple_size, write_external_pid, AtomTable, Eterm,
};

/// A distribution seam that records every routed signal instead of touching a socket.
///
/// The log is shared through an `Rc<RefCell<_>>` because the seam lives behind a
/// `Box<dyn DistSignalRouter>` inside the table and is not downcastable — the test keeps its
/// own handle to inspect what was routed.
struct RecordingRouter {
    signals: Rc<RefCell<Vec<DistSignal>>>,
}

impl DistSignalRouter for RecordingRouter {
    fn route(&mut self, signal: DistSignal) -> Result<()> {
        self.signals.borrow_mut().push(signal);
        Ok(())
    }
}

/// A router plus the log it appends to.
fn recorder() -> (RecordingRouter, Rc<RefCell<Vec<DistSignal>>>) {
    let log = Rc::new(RefCell::new(Vec::new()));
    (
        RecordingRouter {
            signals: Rc::clone(&log),
        },
        log,
    )
}

/// A table plus the atom table its [`SignalAtoms`] were interned from, plus the sysname atom
/// term standing in for the remote node.
fn fixture() -> (ProcessTable, AtomTable, Eterm) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    let node = make_atom(names.intern("remote@host"));
    (ProcessTable::new(atoms), names, node)
}

/// The term for a name, interned in the fixture's atom table if it is not already there, so a
/// test can build a term in the very same atom index space the table's [`SignalAtoms`] use.
fn interning(names: &mut AtomTable, name: &str) -> Eterm {
    make_atom(names.intern(name))
}

/// The term for a name the fixture's [`SignalAtoms::from_table`] interned.
fn named(names: &AtomTable, name: &str) -> Eterm {
    make_atom(names.lookup(name).expect("the fixture interned it"))
}

/// Builds an external pid term on `heap` (the frozen 4-word `[header, node, creation, data]`
/// thing).
fn remote_pid(heap: &mut Heap, node: Eterm, number: u32, serial: u32, creation: u32) -> Eterm {
    // SAFETY: `heap` hands out `external_pid_size_words()` (4) exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(external_pid_size_words()).expect("the heap has room") };
    // SAFETY: `p` addresses the four words just handed out and `write_external_pid` writes all
    // of them before the pointer escapes; `node` is a live atom term.
    unsafe { write_external_pid(p, node, number, serial, creation) }
}

/// The payload of `pid`'s single queued message (panics unless exactly one is queued).
fn sole_payload(table: &ProcessTable, pid: ProcessId) -> Eterm {
    let mailbox = table.get(pid).expect("the recipient is live").mailbox();
    assert_eq!(mailbox.len(), 1, "exactly one message is queued");
    mailbox.peek().expect("one message is queued").payload()
}

#[test]
fn an_external_pid_link_is_recorded_and_routed() {
    let (mut table, _names, node) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let (router, log) = recorder();
    table.set_dist_router(Box::new(router));

    let a = table.spawn_default().expect("a");
    let ext = remote_pid(&mut heap, node, 11, 22, 33);
    let rpid = RemotePid::from_eterm(ext).expect("an external pid");
    assert!(matches!(PidTarget::from_eterm(ext), Ok(PidTarget::Remote(r)) if r == rpid));

    table.link_term(a, ext).expect("a is live");
    assert_eq!(table.remote_links(a), &[rpid]);
    assert_eq!(
        log.borrow().as_slice(),
        &[DistSignal::Link { from: a, to: rpid }]
    );

    // Idempotent: the *local record* is deduped, so a re-link adds nothing. (The control
    // signal is re-sent, matching the refcounted remote-link bookkeeping the C keeps.)
    table.link_term(a, ext).expect("a is live");
    assert_eq!(table.remote_links(a), &[rpid]);
    assert_eq!(
        log.borrow().as_slice(),
        &[
            DistSignal::Link { from: a, to: rpid },
            DistSignal::Link { from: a, to: rpid },
        ]
    );

    // ... and unlinking drops the record and routes `DOP_UNLINK_ID`.
    table.unlink_term(a, ext).expect("a is live");
    assert!(table.remote_links(a).is_empty());
    assert_eq!(
        log.borrow().as_slice(),
        &[
            DistSignal::Link { from: a, to: rpid },
            DistSignal::Link { from: a, to: rpid },
            DistSignal::Unlink { from: a, to: rpid },
        ]
    );
}

#[test]
fn an_internal_pid_link_stays_local_and_is_not_recorded_as_remote() {
    let (mut table, _names, _node) = fixture();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");

    table.link_term(a, b.to_eterm()).expect("both are live");
    assert!(table.get(a).expect("a").links().contains(b));
    assert!(table.get(b).expect("b").links().contains(a));
    assert!(table.remote_links(a).is_empty(), "a local link is not remote");
    assert!(table.remote_monitors(a).is_empty());

    table.unlink_term(a, b.to_eterm()).expect("both are live");
    assert!(!table.get(a).expect("a").links().contains(b));
    assert!(table.remote_links(a).is_empty());
}

#[test]
fn an_external_pid_monitor_is_recorded_and_routed() {
    let (mut table, _names, node) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let (router, log) = recorder();
    table.set_dist_router(Box::new(router));

    let watcher = table.spawn_default().expect("watcher");
    let ext = remote_pid(&mut heap, node, 5, 6, 7);
    let rpid = RemotePid::from_eterm(ext).expect("an external pid");

    let reference = table
        .monitor_term(watcher, ext, MonitorKind::Process)
        .expect("the watcher is live");
    assert_eq!(
        table.remote_monitors(watcher),
        &[RemoteMonitor {
            reference,
            remote: rpid,
            kind: MonitorKind::Process,
        }]
    );
    assert!(
        table.get(watcher).expect("watcher").monitors().is_empty(),
        "a remote monitor never enters the local monitor set"
    );
    assert_eq!(
        log.borrow().as_slice(),
        &[DistSignal::Monitor {
            watcher,
            target: rpid,
            reference,
            kind: MonitorKind::Process,
        }]
    );
}

#[test]
fn terminating_a_process_routes_the_exit_signal_to_its_remote_links() {
    let (mut table, _names, node) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let (router, log) = recorder();
    table.set_dist_router(Box::new(router));

    let a = table.spawn_default().expect("a");
    let ext = remote_pid(&mut heap, node, 2, 3, 4);
    let rpid = RemotePid::from_eterm(ext).expect("an external pid");
    table.link_term(a, ext).expect("a is live");

    let reason = make_small(9);
    table.terminate(a, reason).expect("a is live");
    assert!(!table.contains(a));
    assert_eq!(
        log.borrow().as_slice(),
        &[
            DistSignal::Link { from: a, to: rpid },
            DistSignal::Exit {
                from: a,
                to: rpid,
                reason,
            },
        ]
    );
    assert!(table.remote_links(a).is_empty(), "the record dies with a");
}

#[test]
fn demonitoring_a_remote_monitor_drops_it_and_routes() {
    let (mut table, _names, node) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let (router, log) = recorder();
    table.set_dist_router(Box::new(router));

    let watcher = table.spawn_default().expect("watcher");
    let ext = remote_pid(&mut heap, node, 8, 9, 10);
    let rpid = RemotePid::from_eterm(ext).expect("an external pid");
    let reference = table
        .monitor_term(watcher, ext, MonitorKind::Process)
        .expect("the watcher is live");

    assert!(table.demonitor(watcher, reference).expect("watcher live"));
    assert!(table.remote_monitors(watcher).is_empty());
    assert!(
        !table.demonitor(watcher, reference).expect("watcher live"),
        "a second demonitor finds nothing"
    );
    assert_eq!(
        log.borrow().as_slice(),
        &[
            DistSignal::Monitor {
                watcher,
                target: rpid,
                reference,
                kind: MonitorKind::Process,
            },
            DistSignal::Demonitor {
                watcher,
                target: rpid,
                reference,
            },
        ]
    );
}

#[test]
fn send_exit_term_routes_remote_and_delegates_local() {
    let (mut table, names, node) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let (router, log) = recorder();
    table.set_dist_router(Box::new(router));

    let from = table.spawn_default().expect("from");
    let local = table.spawn_default().expect("local");
    let ext = remote_pid(&mut heap, node, 1, 2, 3);
    let rpid = RemotePid::from_eterm(ext).expect("an external pid");

    let reason = make_small(42);
    table
        .send_exit_term(from, ext, reason)
        .expect("from is live");
    assert_eq!(
        log.borrow().as_slice(),
        &[DistSignal::Exit {
            from,
            to: rpid,
            reason,
        }]
    );

    // A local target still goes through the local signal path.
    table.get_mut(local).expect("local").set_trap_exit(true);
    table
        .send_exit_term(from, local.to_eterm(), reason)
        .expect("both are live");
    assert!(table.contains(local), "a trapping process survives");
    let payload = sole_payload(&table, local);
    // SAFETY: `payload` is the live `{'EXIT', From, Reason}` tuple the table built on the
    // local process's heap; the three element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), named(&names, "EXIT"));
        assert_eq!(tuple_element(payload, 1), from.to_eterm());
        assert_eq!(tuple_element(payload, 2), reason);
    }
}

#[test]
fn node_down_queues_a_down_message_on_each_watcher() {
    let (mut table, mut names, node) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let (router, log) = recorder();
    table.set_dist_router(Box::new(router));

    let watcher = table.spawn_default().expect("watcher");
    let ext = remote_pid(&mut heap, node, 7, 8, 9);
    let rpid = RemotePid::from_eterm(ext).expect("an external pid");
    let reference = table
        .monitor_term(watcher, ext, MonitorKind::Process)
        .expect("the watcher is live");

    let reason = interning(&mut names, "noconnection");
    table.node_down(node, reason).expect("the sweep succeeds");

    assert!(table.remote_monitors(watcher).is_empty(), "the record is purged");
    let payload = sole_payload(&table, watcher);
    // SAFETY: `payload` is the live `{'DOWN', Ref, process, RemotePid, Reason}` tuple the
    // table built on the watcher's heap; the five element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), named(&names, "DOWN"));
        assert_eq!(tuple_element(payload, 2), named(&names, "process"));
        assert_eq!(
            RemotePid::from_eterm(tuple_element(payload, 3)).expect("the target is an external pid"),
            rpid
        );
        assert_eq!(tuple_element(payload, 4), reason);
        // Element 1 is the ref thing; its last payload word encodes the reference id.
        let words = boxed_val(tuple_element(payload, 1));
        assert_eq!(signed_value(*words.add(3)), reference.id() as i64);
    }

    // The only signal routed was the monitor the watcher installed.
    assert_eq!(log.borrow().len(), 1);
}

#[test]
fn node_down_terminates_a_non_trapping_linked_peer() {
    let (mut table, mut names, node) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let (router, log) = recorder();
    table.set_dist_router(Box::new(router));

    let peer = table.spawn_default().expect("peer");
    let ext = remote_pid(&mut heap, node, 4, 4, 4);
    let rpid = RemotePid::from_eterm(ext).expect("an external pid");
    table.link_term(peer, ext).expect("the peer is live");

    let reason = interning(&mut names, "noconnection");
    table.node_down(node, reason).expect("the sweep succeeds");

    assert!(
        !table.contains(peer),
        "a non-trapping peer follows the node down with the node-down reason"
    );
    assert_eq!(
        log.borrow().as_slice(),
        &[
            DistSignal::Link {
                from: peer,
                to: rpid,
            },
            DistSignal::Exit {
                from: peer,
                to: rpid,
                reason,
            },
        ],
        "the dying peer's own remote links are routed as exits"
    );
}

#[test]
fn node_down_delivers_an_exit_message_to_a_trapping_peer() {
    let (mut table, mut names, node) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let (router, log) = recorder();
    table.set_dist_router(Box::new(router));

    let peer = table.spawn_default().expect("peer");
    let ext = remote_pid(&mut heap, node, 12, 13, 14);
    let rpid = RemotePid::from_eterm(ext).expect("an external pid");
    table.get_mut(peer).expect("peer").set_trap_exit(true);
    table.link_term(peer, ext).expect("the peer is live");

    let reason = interning(&mut names, "noconnection");
    table.node_down(node, reason).expect("the sweep succeeds");

    assert!(table.contains(peer), "the trapping peer survives");
    assert!(table.remote_links(peer).is_empty(), "the record is purged");
    let payload = sole_payload(&table, peer);
    // SAFETY: `payload` is the live `{'EXIT', RemotePid, Reason}` tuple the table built on the
    // peer's heap; the three element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), named(&names, "EXIT"));
        assert_eq!(
            RemotePid::from_eterm(tuple_element(payload, 1))
                .expect("the signaller is an external pid"),
            rpid
        );
        assert_eq!(tuple_element(payload, 2), reason);
    }
    assert_eq!(log.borrow().len(), 1, "only the link was routed");
}

#[test]
fn a_node_down_only_touches_records_of_that_node() {
    let (mut table, mut names, node) = fixture();
    let mut heap = Heap::new(64).expect("test heap");

    let other_node = make_atom(names.intern("elsewhere@host"));
    let watcher = table.spawn_default().expect("watcher");
    let on_node = remote_pid(&mut heap, node, 1, 1, 1);
    let elsewhere = remote_pid(&mut heap, other_node, 2, 2, 2);
    let elsewhere_pid = RemotePid::from_eterm(elsewhere).expect("an external pid");
    let reference = table
        .monitor_term(watcher, on_node, MonitorKind::Process)
        .expect("the watcher is live");
    let survivor = table
        .monitor_term(watcher, elsewhere, MonitorKind::Process)
        .expect("the watcher is live");

    table
        .node_down(node, interning(&mut names, "noconnection"))
        .expect("the sweep succeeds");

    assert_eq!(
        table.remote_monitors(watcher),
        &[RemoteMonitor {
            reference: survivor,
            remote: elsewhere_pid,
            kind: MonitorKind::Process,
        }],
        "a monitor on another node is untouched"
    );
    // Exactly one `{'DOWN', …}` was queued — for the dead node only.
    let mailbox = table.get(watcher).expect("watcher").mailbox();
    assert_eq!(mailbox.len(), 1);
    let payload = mailbox.peek().expect("one message").payload();
    // SAFETY: `payload` is the live `{'DOWN', …}` tuple on the watcher's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(signed_value(*boxed_val(tuple_element(payload, 1)).add(3)), reference.id() as i64);
    }
}

#[test]
fn a_remote_pid_round_trips_through_a_heap() {
    let (_table, _names, node) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let ext = remote_pid(&mut heap, node, 0xAAAA_BBBB, 0xCCCC_DDDD, 0x1234_5678);
    let rpid = RemotePid::from_eterm(ext).expect("an external pid");

    assert_eq!(rpid.node(), node);
    assert_eq!(rpid.number(), 0xAAAA_BBBB);
    assert_eq!(rpid.serial(), 0xCCCC_DDDD);
    assert_eq!(rpid.creation(), 0x1234_5678);

    // Materialising it on a fresh heap reproduces exactly the same parts.
    let mut other = Heap::new(8).expect("test heap");
    let rebuilt = rpid.to_eterm(&mut other).expect("the thing fits");
    // SAFETY: `rebuilt` is the live four-word external pid thing `to_eterm` just wrote.
    assert!(unsafe { is_external_pid(rebuilt) });
    assert_eq!(RemotePid::from_eterm(rebuilt).expect("an external pid"), rpid);

    // An internal pid is a *local* target, and is not an external pid.
    let internal = ProcessId::new(4, 0).to_eterm();
    assert!(matches!(
        RemotePid::from_eterm(internal),
        Err(BeamError::BadArg)
    ));
    assert!(
        matches!(
            PidTarget::from_eterm(internal),
            Ok(PidTarget::Local(pid)) if pid == ProcessId::new(4, 0)
        ),
        "an internal pid is a local target"
    );

    // Neither is a non-pid term.
    assert!(matches!(
        PidTarget::from_eterm(make_small(3)),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        RemotePid::from_eterm(make_small(3)),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn a_table_without_a_router_still_records_remote_relationships() {
    let (mut table, _names, node) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    assert!(!table.has_dist_router());

    let a = table.spawn_default().expect("a");
    let ext = remote_pid(&mut heap, node, 1, 2, 3);
    let rpid = RemotePid::from_eterm(ext).expect("an external pid");

    // No seam: the local record stands and nothing panics.
    table.link_term(a, ext).expect("a is live");
    let reference = table
        .monitor_term(a, ext, MonitorKind::Process)
        .expect("a is live");
    assert_eq!(table.remote_links(a), &[rpid]);
    assert_eq!(table.remote_monitors(a).len(), 1);

    // Installing a seam afterwards routes the *next* change.
    let (router, log) = recorder();
    table.set_dist_router(Box::new(router));
    assert!(table.has_dist_router());
    assert!(table.demonitor(a, reference).expect("a is live"));
    assert_eq!(
        log.borrow().as_slice(),
        &[DistSignal::Demonitor {
            watcher: a,
            target: rpid,
            reference,
        }]
    );

    // ... and the seam can be taken back out of the table.
    assert!(table.take_dist_router().is_some());
    assert!(!table.has_dist_router());
    table.link_term(a, ext).expect("a is live");
    assert_eq!(
        log.borrow().len(),
        1,
        "a table with no seam records without routing"
    );
}
