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

//! Integration evidence for the **inbound (receipt)** half of the distributed
//! link/monitor/exit/unlink/group-leader families.
//!
//! The outbound half is pinned by `tests/remote.rs`: a locally-initiated relationship change
//! records its half and hands a [`DistSignal`] to the distribution seam. This file pins the other
//! direction — a `DOP_*` a peer sent us, decoded by `beam_rt` and handed to
//! [`ProcessTable`] as its already-parsed pieces.
//!
//! The one property every inbound entry point must have and the C's receipt arms do have
//! (`dist.c:2258` `break; /* Done */`) is that it **never echoes the opcode it received**: the
//! installed [`DistSignalRouter`] is a recording seam, and every case here asserts it stayed
//! **empty** for the signal the API consumed. The only routed signal allowed in these tests is
//! the one a *locally-initiated* setup step produced (e.g. the `DOP_MONITOR_P` an outgoing
//! [`ProcessTable::monitor_term`] emits before the inbound DOWN is applied).
//!
//! Semantics are the C's: `dist.c`'s receipt switch (link/unlink/monitor/exit/group-leader arms)
//! and `erl_proc_sig_queue.c`'s `handle_exit_signal` (the link-exit vs `exit/2` trap rules). The
//! [`RemoteWatcher`] incoming-record table is pinned here directly too: a record is resolved by
//! the watched process's death or by a `DOP_DEMONITOR_P` receipt
//! ([`ProcessTable::demonitor_from_remote`]), and a later receipt never resurrects it.

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
/// `Box<dyn DistSignalRouter>` inside the table and is not downcastable — the test keeps its own
/// handle to inspect what was routed (and, crucially, to prove **nothing** was).
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

/// A table with a recording distribution seam installed, plus the atom table its [`SignalAtoms`]
/// were interned from, plus the sysname atom term standing in for the remote node.
fn fixture() -> (
    ProcessTable,
    AtomTable,
    Eterm,
    Rc<RefCell<Vec<DistSignal>>>,
) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    let node = make_atom(names.intern("remote@host"));
    let mut table = ProcessTable::new(atoms);
    let (router, log) = recorder();
    table.set_dist_router(Box::new(router));
    (table, names, node, log)
}

/// The term for a name the fixture's [`SignalAtoms::from_table`] interned.
fn named(names: &AtomTable, name: &str) -> Eterm {
    make_atom(names.lookup(name).expect("the fixture interned it"))
}

/// The term for a name, interned in the fixture's atom table if it is not already there, so a
/// test can build a term in the very same atom index space the table's [`SignalAtoms`] use.
fn interning(names: &mut AtomTable, name: &str) -> Eterm {
    make_atom(names.intern(name))
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

/// A local `trap_exit` peer, so a death's reason can be observed.
fn trapping_peer(table: &mut ProcessTable) -> ProcessId {
    let peer = table.spawn_default().expect("peer");
    table.get_mut(peer).expect("peer").set_trap_exit(true);
    peer
}

/// The reason `peer` was handed as `{'EXIT', Dead, Reason}` when `dead` died.
fn peer_sees_exit(
    names: &AtomTable,
    table: &ProcessTable,
    peer: ProcessId,
    dead: ProcessId,
) -> Eterm {
    let payload = sole_payload(table, peer);
    // SAFETY: `payload` is the live `{'EXIT', From, Reason}` tuple the table built on the peer's
    // heap; the three element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), named(names, "EXIT"));
        assert_eq!(tuple_element(payload, 1), dead.to_eterm());
        tuple_element(payload, 2)
    }
}

/// A process-death observer that records `(pid, reason)` for every death.
///
/// It is how a termination reason is observed without a linked peer: a link cascade's reason is
/// the crate's *outbound* propagation rule (a separate concern), so this test reads the dying
/// process's own reason instead. Only an **immediate** reason may be retained (an atom here) —
/// a heap-resident one would dangle once the dying heap is released, exactly as
/// [`ProcessExit::reason`] documents.
struct DeathLog {
    deaths: Rc<RefCell<Vec<(ProcessId, Eterm)>>>,
}

impl ProcessExitObserver for DeathLog {
    fn process_exited(&mut self, exit: &ProcessExit<'_>) {
        self.deaths.borrow_mut().push((exit.pid(), exit.reason()));
    }
}

// --- DOP_LINK ---------------------------------------------------------------

#[test]
fn record_remote_link_records_and_routes_nothing() {
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let to = table.spawn_default().expect("to");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 11, 22, 33)).expect("external pid");

    table.record_remote_link(to, rpid).expect("to is live");
    assert_eq!(table.remote_links(to), &[rpid], "the incoming link is recorded");
    assert!(
        log.borrow().is_empty(),
        "an inbound DOP_LINK is never echoed (`dist.c:2258` `break; /* Done */`)"
    );

    // Idempotent, exactly like `link_term`'s remote branch: a re-sent DOP_LINK adds nothing.
    table.record_remote_link(to, rpid).expect("to is live");
    assert_eq!(table.remote_links(to), &[rpid]);
    assert!(log.borrow().is_empty());

    // ... and it is deduplicated per local process only.
    let other = table.spawn_default().expect("other");
    let second = RemotePid::from_eterm(remote_pid(&mut heap, node, 44, 55, 66)).expect("ext");
    table.record_remote_link(other, rpid).expect("other is live");
    table.record_remote_link(other, second).expect("other is live");
    assert_eq!(table.remote_links(other), &[rpid, second]);
    assert_eq!(table.remote_links(to), &[rpid]);
    assert!(log.borrow().is_empty());
}

#[test]
fn record_remote_link_of_an_unknown_process_is_term_not_found() {
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 1, 2, 3)).expect("external pid");
    let dead = ProcessId::new(77, 0);

    assert!(matches!(
        table.record_remote_link(dead, rpid),
        Err(BeamError::TermNotFound)
    ));
    assert!(table.remote_links(dead).is_empty());
    assert!(log.borrow().is_empty());
}

// --- DOP_EXIT / DOP_PAYLOAD_EXIT (the link-exit family, `untrappable == false`) ---------

#[test]
fn a_link_exit_removes_the_link_and_traps_for_a_trapping_receiver() {
    let (mut table, names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let to = table.spawn_default().expect("to");
    table.get_mut(to).expect("to").set_trap_exit(true);
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 9, 9, 9)).expect("external pid");
    table.record_remote_link(to, rpid).expect("to is live");
    log.borrow_mut().clear();

    let reason = make_small(7);
    table
        .apply_remote_exit_signal(to, rpid, reason, false)
        .expect("to is live");

    assert!(table.contains(to), "a trapping receiver survives");
    assert!(
        table.remote_links(to).is_empty(),
        "the link record is removed first (`erl_proc_sig_queue.c:4674-4696`)"
    );
    let payload = sole_payload(&table, to);
    // SAFETY: `payload` is the live `{'EXIT', RemotePid, Reason}` tuple on `to`'s heap.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), named(&names, "EXIT"));
        assert_eq!(
            RemotePid::from_eterm(tuple_element(payload, 1)).expect("an external pid"),
            rpid,
            "the body's `From` is the remote external pid"
        );
        assert_eq!(tuple_element(payload, 2), reason);
    }
    assert!(
        log.borrow().is_empty(),
        "an inbound DOP_EXIT is never re-routed"
    );
}

#[test]
fn a_link_exit_traps_for_every_reason_including_normal_and_kill() {
    // A link exit is trappable for *every* reason: the C guards its `killed` rewrite — and every
    // other special case — behind `op == ERTS_SIG_Q_OP_EXIT` (`erl_proc_sig_queue.c:4740`), which
    // the link-exit family never is, so a trapping receiver takes the `{'EXIT', RemotePid, Reason}`
    // message verbatim — `normal` and `kill` included.
    let (mut table, names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 71, 72, 73)).expect("ext");

    for reason in [named(&names, "normal"), named(&names, "kill"), make_small(7)] {
        // A fresh receiver per reason, so `sole_payload` sees exactly one message.
        let to = table.spawn_default().expect("to");
        table.get_mut(to).expect("to").set_trap_exit(true);
        table.record_remote_link(to, rpid).expect("to is live");
        table
            .apply_remote_exit_signal(to, rpid, reason, false)
            .expect("to is live");

        assert!(table.contains(to), "a trapping receiver survives");
        assert!(
            table.remote_links(to).is_empty(),
            "the link record is removed first"
        );
        let payload = sole_payload(&table, to);
        // SAFETY: `payload` is the live `{'EXIT', RemotePid, Reason}` tuple on `to`'s heap.
        unsafe {
            assert_eq!(tuple_size(payload), 3);
            assert_eq!(tuple_element(payload, 0), named(&names, "EXIT"));
            assert_eq!(
                RemotePid::from_eterm(tuple_element(payload, 1)).expect("an external pid"),
                rpid
            );
            assert_eq!(
                tuple_element(payload, 2),
                reason,
                "the link-exit reason is handed over unchanged (never `killed`)"
            );
        }
    }
    assert!(log.borrow().is_empty());
}

#[test]
fn a_link_exit_terminates_a_non_trapping_receiver_with_its_reason() {
    let (mut table, names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let to = table.spawn_default().expect("to");
    let peer = trapping_peer(&mut table);
    table.link(to, peer).expect("both are live");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 3, 3, 3)).expect("external pid");
    table.record_remote_link(to, rpid).expect("to is live");
    log.borrow_mut().clear();

    let reason = make_small(1234);
    table
        .apply_remote_exit_signal(to, rpid, reason, false)
        .expect("to is live");

    assert!(!table.contains(to), "a non-trapping receiver is terminated");
    assert_eq!(
        peer_sees_exit(&names, &table, peer, to),
        reason,
        "the link exit terminates with the reason unchanged"
    );
    assert!(log.borrow().is_empty());
}

#[test]
fn a_link_exit_of_kill_terminates_with_kill_not_killed() {
    // The `reason = am_killed` rewrite is guarded by `op == ERTS_SIG_Q_OP_EXIT`
    // (`erl_proc_sig_queue.c:4740`/`:4768`), so the *link-exit* family keeps `kill`. Oracle
    // evidence: `spawn(fun() -> exit(kill) end)` yields a DOWN reason of `kill` (never
    // `killed`), and a trapping link peer receives `{'EXIT', Dead, kill}`.
    let (mut table, names, node, log) = fixture();
    let deaths = Rc::new(RefCell::new(Vec::new()));
    table.set_process_exit_observer(Box::new(DeathLog {
        deaths: Rc::clone(&deaths),
    }));
    let mut heap = Heap::new(64).expect("test heap");
    let to = table.spawn_default().expect("to");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 5, 5, 5)).expect("external pid");
    table.record_remote_link(to, rpid).expect("to is live");
    log.borrow_mut().clear();

    let kill = named(&names, "kill");
    table
        .apply_remote_exit_signal(to, rpid, kill, false)
        .expect("to is live");

    assert!(!table.contains(to));
    assert_eq!(
        deaths.borrow().as_slice(),
        &[(to, kill)],
        "a link exit of `kill` is NOT rewritten to `killed`"
    );
    assert!(log.borrow().is_empty());
}

#[test]
fn a_link_exit_of_normal_is_ignored_by_a_non_trapping_receiver() {
    let (mut table, names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let to = table.spawn_default().expect("to");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 6, 6, 6)).expect("external pid");
    table.record_remote_link(to, rpid).expect("to is live");
    log.borrow_mut().clear();

    table
        .apply_remote_exit_signal(to, rpid, named(&names, "normal"), false)
        .expect("to is live");

    assert!(table.contains(to), "a normal link exit is ignored");
    assert!(table.get(to).expect("to").mailbox().is_empty());
    assert!(table.remote_links(to).is_empty(), "the link still goes");
    assert!(log.borrow().is_empty());
}

#[test]
fn a_link_exit_with_no_recorded_link_is_ignored_entirely() {
    // `ignore = destroy = !0` when the link-tree lookup misses
    // (`erl_proc_sig_queue.c:4674-4680`).
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let to = table.spawn_default().expect("to");
    table.get_mut(to).expect("to").set_trap_exit(true);
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 8, 8, 8)).expect("external pid");

    // No `record_remote_link` call: the signal must be dropped, even for a trapping receiver.
    table
        .apply_remote_exit_signal(to, rpid, make_small(99), false)
        .expect("to is live");

    assert!(table.contains(to));
    assert!(table.get(to).expect("to").mailbox().is_empty());
    assert!(log.borrow().is_empty());
}

// --- DOP_EXIT2 / DOP_PAYLOAD_EXIT2 (the `exit/2` family, `untrappable == true`) ----------

#[test]
fn exit_2_delivers_an_exit_message_to_a_trapping_receiver_and_touches_no_link() {
    let (mut table, names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let to = table.spawn_default().expect("to");
    table.get_mut(to).expect("to").set_trap_exit(true);
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 12, 12, 12)).expect("external pid");
    table.record_remote_link(to, rpid).expect("to is live");
    log.borrow_mut().clear();

    let reason = make_small(314);
    table
        .apply_remote_exit_signal(to, rpid, reason, true)
        .expect("to is live");

    assert!(table.contains(to), "a trapping receiver survives");
    assert_eq!(
        table.remote_links(to),
        &[rpid],
        "`erlang:exit/2` consults no link — the recorded one is untouched"
    );
    let payload = sole_payload(&table, to);
    // SAFETY: `payload` is the live `{'EXIT', RemotePid, Reason}` tuple on `to`'s heap.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), named(&names, "EXIT"));
        assert_eq!(
            RemotePid::from_eterm(tuple_element(payload, 1)).expect("an external pid"),
            rpid
        );
        assert_eq!(tuple_element(payload, 2), reason);
    }
    assert!(log.borrow().is_empty());
}

#[test]
fn exit_2_kill_terminates_a_trapping_receiver_as_killed() {
    let (mut table, names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let to = table.spawn_default().expect("to");
    table.get_mut(to).expect("to").set_trap_exit(true);
    let peer = trapping_peer(&mut table);
    table.link(to, peer).expect("both are live");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 13, 13, 13)).expect("external pid");
    log.borrow_mut().clear();

    table
        .apply_remote_exit_signal(to, rpid, named(&names, "kill"), true)
        .expect("to is live");

    assert!(
        !table.contains(to),
        "`kill` is untrappable in the exit/2 family — even a trapping receiver dies"
    );
    assert_eq!(
        peer_sees_exit(&names, &table, peer, to),
        named(&names, "killed"),
        "the exit/2 family rewrites `kill` to `killed` (`erl_proc_sig_queue.c:4768`)"
    );
    assert!(log.borrow().is_empty());
}

#[test]
fn an_unknown_remote_exit_target_is_term_not_found() {
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 1, 1, 1)).expect("external pid");
    let dead = ProcessId::new(101, 0);

    for untrappable in [false, true] {
        assert!(matches!(
            table.apply_remote_exit_signal(dead, rpid, make_small(1), untrappable),
            Err(BeamError::TermNotFound)
        ));
    }
    assert!(log.borrow().is_empty());
}

// --- DOP_UNLINK_ID / DOP_UNLINK_ID_ACK --------------------------------------

#[test]
fn unlink_from_remote_removes_the_link_and_returns_the_ack_id() {
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let to = table.spawn_default().expect("to");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 21, 22, 23)).expect("ext");
    table.record_remote_link(to, rpid).expect("to is live");
    log.borrow_mut().clear();

    // `{DOP_UNLINK_ID, Id, From(remote), To(local)}` — `Id` is the FIRST element
    // (`dist.c:2284-2290`); the id is echoed into the required ACK.
    let ack = table
        .unlink_from_remote(to, rpid, 4242)
        .expect("to is live");
    assert_eq!(ack, Some(4242), "id != 0 requires a DOP_UNLINK_ID_ACK");
    assert!(table.remote_links(to).is_empty(), "the link is removed");
    assert!(
        log.borrow().is_empty(),
        "an inbound DOP_UNLINK_ID answers with DOP_UNLINK_ID_ACK (a different opcode) and \
         never re-routes a plain DOP_UNLINK_ID"
    );

    // `id == 0` (a peer that does not understand the handshake) requires no ACK
    // (`erl_proc_sig_queue.c:2753-2760`).
    table.record_remote_link(to, rpid).expect("to is live");
    assert_eq!(
        table.unlink_from_remote(to, rpid, 0).expect("to is live"),
        None
    );
    assert!(table.remote_links(to).is_empty());

    // An unlink of a link that is not recorded still answers (the C replies unconditionally)
    // and changes nothing.
    assert_eq!(
        table.unlink_from_remote(to, rpid, 7).expect("to is live"),
        Some(7)
    );
    assert!(log.borrow().is_empty());

    // The link to another remote pid — and another local process's record — are untouched.
    let other = RemotePid::from_eterm(remote_pid(&mut heap, node, 31, 32, 33)).expect("ext");
    let peer = table.spawn_default().expect("peer");
    table.record_remote_link(to, other).expect("to is live");
    table.record_remote_link(peer, other).expect("peer is live");
    table.unlink_from_remote(to, rpid, 8).expect("to is live");
    assert_eq!(table.remote_links(to), &[other]);
    assert_eq!(table.remote_links(peer), &[other]);
    assert!(log.borrow().is_empty());
}

#[test]
fn unlink_from_remote_of_an_unknown_process_is_term_not_found() {
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 2, 2, 2)).expect("ext");
    assert!(matches!(
        table.unlink_from_remote(ProcessId::new(9, 0), rpid, 1),
        Err(BeamError::TermNotFound)
    ));
    assert!(log.borrow().is_empty());
}

#[test]
fn unlink_ack_from_remote_validates_liveness_and_routes_nothing() {
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let to = table.spawn_default().expect("to");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 41, 42, 43)).expect("ext");

    // The crate models no pending unlink-id operations (the local `DistSignal::Unlink` carries no
    // id), so the ACK only validates that `to` is live and is otherwise a no-op.
    table
        .unlink_ack_from_remote(to, rpid, 0xBEEF)
        .expect("to is live");
    assert!(log.borrow().is_empty());
    assert!(table.get(to).expect("to").mailbox().is_empty());
    assert!(table.remote_links(to).is_empty());

    assert!(matches!(
        table.unlink_ack_from_remote(ProcessId::new(10, 0), rpid, 1),
        Err(BeamError::TermNotFound)
    ));
    assert!(log.borrow().is_empty());
}

// --- DOP_MONITOR_P / DOP_DEMONITOR_P ----------------------------------------

#[test]
fn monitor_from_remote_records_and_demonitor_removes() {
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let watched = table.spawn_default().expect("watched");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 51, 52, 53)).expect("ext");

    // The reference is minted locally (the wire carries it; parsing it is `beam_rt`'s job).
    let reference = table.next_reference();
    table
        .monitor_from_remote(watched, rpid, reference, MonitorKind::Process)
        .expect("watched is live");
    assert_eq!(
        table.remote_watchers(watched),
        &[RemoteWatcher {
            reference,
            watcher: rpid,
            kind: MonitorKind::Process,
        }]
    );
    assert!(
        table.get(watched).expect("watched").monitored_by().is_empty(),
        "an incoming remote watcher never enters the local MonitoredBy set"
    );
    assert!(
        log.borrow().is_empty(),
        "an inbound DOP_MONITOR_P is never echoed (`dist.c:2378`)"
    );

    // A re-sent DOP_MONITOR_P for the same reference replaces the record (the C's origin tree is
    // keyed by the ref).
    table
        .monitor_from_remote(watched, rpid, reference, MonitorKind::Port)
        .expect("watched is live");
    assert_eq!(table.remote_watchers(watched).len(), 1);
    assert_eq!(
        table.remote_watchers(watched)[0].kind,
        MonitorKind::Port,
        "the record is replaced, not duplicated"
    );

    // A second, distinct reference coexists.
    let second = table.next_reference();
    table
        .monitor_from_remote(watched, rpid, second, MonitorKind::Process)
        .expect("watched is live");
    assert_eq!(table.remote_watchers(watched).len(), 2);
    assert!(log.borrow().is_empty());

    table
        .demonitor_from_remote(watched, reference)
        .expect("watched is live");
    assert_eq!(table.remote_watchers(watched).len(), 1);
    assert!(matches!(
        table.remote_watchers(watched)[0].reference,
        r if r == second
    ));
    assert!(log.borrow().is_empty(), "an inbound DOP_DEMONITOR_P is never echoed");

    // Removing an absent reference is a no-op; removing the last one empties the entry.
    table
        .demonitor_from_remote(watched, reference)
        .expect("watched is live");
    table
        .demonitor_from_remote(watched, second)
        .expect("watched is live");
    assert!(table.remote_watchers(watched).is_empty());
    assert!(log.borrow().is_empty());
}

#[test]
fn an_incoming_watcher_record_dies_with_the_watched_process() {
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let watched = table.spawn_default().expect("watched");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 61, 62, 63)).expect("ext");
    let reference = table.next_reference();
    table
        .monitor_from_remote(watched, rpid, reference, MonitorKind::Process)
        .expect("watched is live");

    table.terminate(watched, make_small(0)).expect("watched is live");
    assert!(!table.contains(watched));
    assert!(table.remote_watchers(watched).is_empty());
    assert!(log.borrow().is_empty());
}

#[test]
fn demonitor_from_remote_removes_only_its_reference_on_that_process() {
    // `demonitor_from_remote` is the `DOP_DEMONITOR_P` receipt on its own (not merely a step of
    // the monitor round-trip above): it drops exactly the matching reference of the named local
    // process, is an `Ok` no-op for one that is already gone (no "resurrection"), and never
    // touches another local process holding the very same reference.
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let watched = table.spawn_default().expect("watched");
    let other = table.spawn_default().expect("other");
    let first = RemotePid::from_eterm(remote_pid(&mut heap, node, 151, 152, 153)).expect("ext");
    let second = RemotePid::from_eterm(remote_pid(&mut heap, node, 161, 162, 163)).expect("ext");

    let r1 = table.next_reference();
    let r2 = table.next_reference();
    table
        .monitor_from_remote(watched, first, r1, MonitorKind::Process)
        .expect("watched is live");
    table
        .monitor_from_remote(watched, second, r2, MonitorKind::Process)
        .expect("watched is live");
    // The same reference recorded for a **different** local process is an independent record.
    table
        .monitor_from_remote(other, first, r1, MonitorKind::Process)
        .expect("other is live");

    table
        .demonitor_from_remote(watched, r1)
        .expect("watched is live");
    assert_eq!(
        table.remote_watchers(watched),
        &[RemoteWatcher {
            reference: r2,
            watcher: second,
            kind: MonitorKind::Process,
        }],
        "only the matching reference on the named process goes"
    );
    assert_eq!(
        table.remote_watchers(other).len(),
        1,
        "another local process's identical reference is untouched"
    );
    assert_eq!(table.remote_watchers(other)[0].watcher, first);

    // Removing a reference that is already gone is an `Ok` no-op, not an error, and does not
    // resurrect the record.
    table
        .demonitor_from_remote(watched, r1)
        .expect("watched is live");
    assert_eq!(
        table.remote_watchers(watched),
        &[RemoteWatcher {
            reference: r2,
            watcher: second,
            kind: MonitorKind::Process,
        }],
        "a re-sent DOP_DEMONITOR_P resurrects nothing"
    );

    assert!(matches!(
        table.demonitor_from_remote(ProcessId::new(202, 0), r2),
        Err(BeamError::TermNotFound)
    ));
    assert!(log.borrow().is_empty(), "an inbound DOP_DEMONITOR_P never routes");
}

#[test]
fn an_incoming_watcher_record_is_dropped_by_death_and_by_demonitor() {
    // The `RemoteWatcher` incoming-record table resolves by the two paths the C has: the watched
    // process's death drops its whole entry (the monitor teardown in `erl_proc_sig_queue.c`), and
    // a `DOP_DEMONITOR_P` receipt drops the one reference it names. Neither ever resurfaces.
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let demonitored = table.spawn_default().expect("demonitored");
    let dying = table.spawn_default().expect("dying");
    let watcher = RemotePid::from_eterm(remote_pid(&mut heap, node, 171, 172, 173)).expect("ext");

    // The very same reference names an incoming watcher of each of the two processes.
    let reference = table.next_reference();
    table
        .monitor_from_remote(demonitored, watcher, reference, MonitorKind::Process)
        .expect("demonitored is live");
    table
        .monitor_from_remote(dying, watcher, reference, MonitorKind::Port)
        .expect("dying is live");

    // 1. a DOP_DEMONITOR_P receipt drops the record of `demonitored` only.
    table
        .demonitor_from_remote(demonitored, reference)
        .expect("demonitored is live");
    assert!(
        table.remote_watchers(demonitored).is_empty(),
        "the demonitored record is gone"
    );
    assert_eq!(
        table.remote_watchers(dying),
        &[RemoteWatcher {
            reference,
            watcher,
            kind: MonitorKind::Port,
        }],
        "the sibling process's record under the same reference survives"
    );
    // ... and it is not resurrected by a re-sent DOP_DEMONITOR_P.
    table
        .demonitor_from_remote(demonitored, reference)
        .expect("demonitored is live");
    assert!(table.remote_watchers(demonitored).is_empty());

    // 2. the watched process's death drops its record wholesale.
    table.terminate(dying, make_small(0)).expect("dying is live");
    assert!(!table.contains(dying));
    assert!(
        table.remote_watchers(dying).is_empty(),
        "a dead process keeps no incoming-watcher record"
    );
    // ... nor can a later receipt resurrect one for the dead pid.
    assert!(matches!(
        table.monitor_from_remote(dying, watcher, reference, MonitorKind::Process),
        Err(BeamError::TermNotFound)
    ));
    assert!(table.remote_watchers(dying).is_empty());

    assert!(log.borrow().is_empty());
}

#[test]
fn remote_monitor_apis_of_an_unknown_process_are_term_not_found() {
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let rpid = RemotePid::from_eterm(remote_pid(&mut heap, node, 71, 72, 73)).expect("ext");
    let dead = ProcessId::new(55, 0);
    let reference = table.next_reference();

    assert!(matches!(
        table.monitor_from_remote(dead, rpid, reference, MonitorKind::Process),
        Err(BeamError::TermNotFound)
    ));
    assert!(matches!(
        table.demonitor_from_remote(dead, reference),
        Err(BeamError::TermNotFound)
    ));
    assert!(matches!(
        table.remote_monitor_down(dead, rpid, reference, make_small(1)),
        Err(BeamError::TermNotFound)
    ));
    assert!(table.remote_watchers(dead).is_empty());
    assert!(log.borrow().is_empty());
}

// --- DOP_MONITOR_P_EXIT / DOP_PAYLOAD_MONITOR_P_EXIT ------------------------

#[test]
fn remote_monitor_down_delivers_the_down_and_drops_the_outgoing_record_without_routing() {
    let (mut table, names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let watcher = table.spawn_default().expect("watcher");
    let ext = remote_pid(&mut heap, node, 81, 82, 83);
    let rpid = RemotePid::from_eterm(ext).expect("external pid");

    // The locally-initiated monitor routed its `DOP_MONITOR_P` — that is the setup, not the
    // inbound receipt under test.
    let reference = table
        .monitor_term(watcher, ext, MonitorKind::Process)
        .expect("the watcher is live");
    assert_eq!(log.borrow().len(), 1);
    log.borrow_mut().clear();

    let reason = make_small(11);
    table
        .remote_monitor_down(watcher, rpid, reference, reason)
        .expect("the watcher is live");

    assert!(
        table.remote_monitors(watcher).is_empty(),
        "the outgoing RemoteMonitor is removed — the removal is the point"
    );
    let payload = sole_payload(&table, watcher);
    // SAFETY: `payload` is the live `{'DOWN', Ref, process, RemotePid, Reason}` tuple on the
    // watcher's heap; the five element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), named(&names, "DOWN"));
        assert_eq!(tuple_element(payload, 2), named(&names, "process"));
        assert_eq!(
            RemotePid::from_eterm(tuple_element(payload, 3)).expect("an external pid"),
            rpid
        );
        assert_eq!(tuple_element(payload, 4), reason);
        let words = boxed_val(tuple_element(payload, 1));
        assert_eq!(signed_value(*words.add(3)), reference.id() as i64);
    }
    assert!(
        log.borrow().is_empty(),
        "an inbound DOWN must not route a DOP_DEMONITOR_P back to the remote node"
    );
}

#[test]
fn remote_monitor_down_with_no_record_delivers_nothing_and_does_not_double_fire() {
    let (mut table, mut names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let watcher = table.spawn_default().expect("watcher");
    let ext = remote_pid(&mut heap, node, 91, 92, 93);
    let rpid = RemotePid::from_eterm(ext).expect("external pid");

    // No outgoing monitor record exists: the C drops the DOWN
    // (`erl_proc_sig_queue.c:6355-6396`).
    let orphan = table.next_reference();
    table
        .remote_monitor_down(watcher, rpid, orphan, make_small(1))
        .expect("the watcher is live");
    assert!(table.get(watcher).expect("watcher").mailbox().is_empty());
    assert!(log.borrow().is_empty());

    // With a record: the DOWN is delivered once, and the record's removal is what stops a later
    // `node_down` sweep from firing a **second** one for the same reference.
    let reference = table
        .monitor_term(watcher, ext, MonitorKind::Process)
        .expect("the watcher is live");
    log.borrow_mut().clear();
    table
        .remote_monitor_down(watcher, rpid, reference, make_small(2))
        .expect("the watcher is live");
    assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 1);

    table
        .node_down(node, interning(&mut names, "noconnection"))
        .expect("the sweep succeeds");
    assert_eq!(
        table.get(watcher).expect("watcher").mailbox().len(),
        1,
        "the record was already resolved, so the node-down sweep does not fire again"
    );
    assert!(log.borrow().is_empty());
}

// --- DOP_GROUP_LEADER -------------------------------------------------------

#[test]
fn set_group_leader_from_remote_records_a_remote_leader() {
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let member = table.spawn_default().expect("member");
    let other = table.spawn_default().expect("other");
    let leader = RemotePid::from_eterm(remote_pid(&mut heap, node, 1, 2, 3)).expect("ext");

    assert_eq!(table.group_leader(member), None, "no leader recorded yet");
    table
        .set_group_leader_from_remote(member, leader)
        .expect("member is live");
    assert_eq!(table.group_leader(member), Some(PidTarget::Remote(leader)));
    assert_eq!(
        table.group_leader(other),
        None,
        "the leader is keyed by the member process"
    );
    assert!(
        log.borrow().is_empty(),
        "an inbound DOP_GROUP_LEADER is never echoed (`dist.c:2889-2900`)"
    );

    // A later DOP_GROUP_LEADER replaces the leader, and a second member can point elsewhere.
    let second = RemotePid::from_eterm(remote_pid(&mut heap, node, 4, 5, 6)).expect("ext");
    table
        .set_group_leader_from_remote(member, second)
        .expect("member is live");
    table
        .set_group_leader_from_remote(other, leader)
        .expect("other is live");
    assert_eq!(table.group_leader(member), Some(PidTarget::Remote(second)));
    assert_eq!(table.group_leader(other), Some(PidTarget::Remote(leader)));
    assert!(log.borrow().is_empty());

    // The entry dies with the member, and an unknown/foreign pid answers `None`.
    table.terminate(member, make_small(0)).expect("member is live");
    assert_eq!(table.group_leader(member), None);
    assert_eq!(table.group_leader(ProcessId::new(120, 0)), None);
    assert_eq!(table.group_leader(other), Some(PidTarget::Remote(leader)));
    assert!(log.borrow().is_empty());
}

#[test]
fn set_group_leader_from_remote_of_an_unknown_process_is_term_not_found() {
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let leader = RemotePid::from_eterm(remote_pid(&mut heap, node, 1, 1, 1)).expect("ext");
    assert!(matches!(
        table.set_group_leader_from_remote(ProcessId::new(130, 0), leader),
        Err(BeamError::TermNotFound)
    ));
    assert!(log.borrow().is_empty());
}

// --- the table's own reference-id space (HAZARD 2, the additive seam) --------

#[test]
fn next_reference_draws_from_the_tables_own_id_space() {
    // The additive closure avenue for HAZARD 2: `next_reference` hands out ids from the very same
    // monotone `RefAllocator` that mints every monitor/alias reference, so a host that adopts it
    // for `make_ref/0`/timers/`seq_trace` can never collide with a monitor reference.
    let (mut table, _names, _node, log) = fixture();
    let owner = table.spawn_default().expect("owner");

    let first = table.next_reference();
    let second = table.next_reference();
    assert_eq!(second.id(), first.id() + 1, "monotone, never reusing an id");

    let alias = table
        .create_alias(owner, UnaliasMode::ExplicitUnalias)
        .expect("owner is live");
    assert_eq!(
        alias.id(),
        second.id() + 1,
        "the same allocator backs next_reference, monitors and aliases"
    );
    assert_ne!(first, alias);
    assert_ne!(second, alias);
    assert!(log.borrow().is_empty(), "minting a reference routes nothing");
}

#[test]
fn an_external_pid_link_partner_round_trips_through_the_inbound_records() {
    // A last end-to-end shape check: the same `RemotePid` parsed from an external pid term is
    // what every inbound record stores and returns.
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    let ext = remote_pid(&mut heap, node, 0xDEAD_BEEF, 0xFEED_FACE, 0x0BAD_F00D);
    // SAFETY: `ext` is the live four-word external pid thing `remote_pid` just wrote.
    assert!(unsafe { is_external_pid(ext) });
    let rpid = RemotePid::from_eterm(ext).expect("an external pid");
    assert_eq!(rpid.node(), node);

    table.record_remote_link(a, rpid).expect("a is live");
    let reference = table.next_reference();
    table
        .monitor_from_remote(b, rpid, reference, MonitorKind::Process)
        .expect("b is live");
    table
        .set_group_leader_from_remote(a, rpid)
        .expect("a is live");

    assert_eq!(table.remote_links(a), &[rpid]);
    assert_eq!(table.remote_watchers(b)[0].watcher, rpid);
    assert_eq!(table.group_leader(a), Some(PidTarget::Remote(rpid)));
    assert!(log.borrow().is_empty());
}
