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

//! Integration evidence for [`DistSignal::MonitorExit`] — the **outbound**
//! `DOP_MONITOR_P_EXIT` a watched local process's death routes to every recorded
//! [`RemoteWatcher`].
//!
//! `tests/remote_inbound.rs` pins the *record* half: an inbound `DOP_MONITOR_P` (and its
//! `DOP_DEMONITOR_P`) is never echoed onto the wire, and a death resolves the incoming-watcher
//! entry. This file pins the other end of that resolution — the signal the resolution
//! **emits**, because it is what makes the peer's `{'DOWN', Ref, process, Watched, Reason}`
//! arrive at all: without it a record is simply dropped when its subject dies, and a remote
//! `erlang:monitor(process, RemotePid)` never gets its DOWN.
//!
//! Ground truth (read-only): `erts_dsig_send_m_exit` (`erts/emulator/beam/dist.c:1497-1519`)
//! builds `TUPLE5(DOP_MONITOR_P_EXIT, Watched, Watcher, Ref, Reason)` — the **watched party
//! comes first**, the reverse of the monitor family's `{DOP_MONITOR_P, watcher, watched, ref}`
//! (`dist.c:1543`). The C's `DFLAG_DIST_MONITOR` support gate and its preference for the 4-tuple
//! `DOP_PAYLOAD_MONITOR_P_EXIT` under `DFLAG_EXIT_PAYLOAD` (`dist.c:1504-1519`) are the
//! *transport's* business, which is why `beam_process` emits exactly one signal per recorded
//! watcher and knows nothing about negotiated flags (see [`DistSignal::MonitorExit`]'s docs).
//!
//! Two properties are the whole point of these cases and are asserted in every one of them:
//!
//! * the reference goes back **in the spelling the table recorded it** ([`RemoteWatcherRef`]) —
//!   a peer that minted its own reference matches its DOWN set on *that* external reference,
//!   which has no faithful `u64` image; and
//! * the reason is the very term the death was handed, which may live in the dying heap, so the
//!   router must serialise it synchronously.
//!
//! Determinism: no sleep, no wall-clock bound and no oracle. Every expectation is derived from
//! the C cited above and from the termination step in `src/ptab.rs`, and the reasons are built
//! from fixed terms on the test's own heaps.

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_common::Result;
use beam_process::prelude::*;
use beam_term::prelude::{
    external_pid_size_words, external_ref_size_words, make_atom, make_small, size_object,
    write_external_pid, write_external_ref, AtomTable, Eterm,
};
use beam_term::compare;

/// A distribution seam that records every routed signal instead of touching a socket.
///
/// The log is shared through an `Rc<RefCell<_>>` because the seam lives behind a
/// `Box<dyn DistSignalRouter>` inside the table and is not downcastable — the test keeps its own
/// handle to inspect what was routed.
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

/// A router-less table plus the atom table its [`SignalAtoms`] were interned from plus the
/// sysname atom term standing in for the remote node.
///
/// This is the shape of a node with **no** distribution transport installed, which is the
/// record-only case [`without_a_router_a_death_is_record_only`] pins.
fn bare() -> (ProcessTable, AtomTable, Eterm) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    let node = make_atom(names.intern("remote@host"));
    (ProcessTable::new(atoms), names, node)
}

/// The same, with a recording distribution seam installed and a handle to its log.
fn fixture() -> (
    ProcessTable,
    AtomTable,
    Eterm,
    Rc<RefCell<Vec<DistSignal>>>,
) {
    let (mut table, names, node) = bare();
    let (router, log) = recorder();
    table.set_dist_router(Box::new(router));
    (table, names, node, log)
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

/// Builds an external reference term on `heap` (the frozen
/// `[header, node, creation, data.ui32]` thing) — the shape a peer's `DOP_MONITOR_P` `Ref`
/// arrives as, and therefore the shape this node must send back verbatim.
fn remote_ref(heap: &mut Heap, node: Eterm, ids: &[u32], creation: u32) -> Eterm {
    // SAFETY: `heap` hands out `external_ref_size_words(ids.len())` exclusively-owned, writable
    // words (and `ids.len() <= MAX_REF_IDS` at every call site here).
    let p = unsafe {
        heap.alloc_words(external_ref_size_words(ids.len()))
            .expect("the heap has room")
    };
    // SAFETY: `p` addresses those words and `write_external_ref` writes all of them before the
    // pointer escapes; `node` is a live atom term.
    unsafe { write_external_ref(p, node, ids, creation) }
}

/// The four payload fields of a routed [`DistSignal::MonitorExit`], panicking on any other
/// variant — so a test that destructures through this helper also proves **no other**
/// [`DistSignal`] variant appeared on the wire.
fn monitor_exit(signal: &DistSignal) -> (ProcessId, RemotePid, RemoteWatcherRef, Eterm) {
    match *signal {
        DistSignal::MonitorExit {
            watched,
            watcher,
            reference,
            reason,
        } => (watched, watcher, reference, reason),
        other => panic!("expected a DistSignal::MonitorExit, got {other:?}"),
    }
}

/// A remote watcher record installed through the **peer-minted** reference spelling
/// (`monitor_from_remote_ref`, the inbound `DOP_MONITOR_P` a real peer sends).
fn record_peer_watcher(
    table: &mut ProcessTable,
    heap: &mut Heap,
    watched: ProcessId,
    node: Eterm,
    pid_fields: (u32, u32, u32),
    ref_ids: &[u32],
    creation: u32,
) -> (RemotePid, RemoteRef) {
    let watcher =
        RemotePid::from_eterm(remote_pid(heap, node, pid_fields.0, pid_fields.1, pid_fields.2))
            .expect("an external pid");
    let reference =
        RemoteRef::from_eterm(remote_ref(heap, node, ref_ids, creation)).expect("an external ref");
    table
        .monitor_from_remote_ref(watched, watcher, reference, MonitorKind::Process)
        .expect("the watched process is live");
    (watcher, reference)
}

#[test]
fn a_death_routes_one_monitor_exit_for_a_peer_minted_record() {
    // The wire shape of the monitor family: the **peer** mints the reference, so the record
    // holds a decomposed `RemoteRef`. The death must send it back verbatim — the peer matches
    // the `{'DOWN', Ref, …}` it will build on *that* reference, and a lossy `u64` image of it
    // does not exist (`dist.c:1497-1519`; the reference is `RemoteWatcherRef`'s reason to be).
    let (mut table, names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let dying = table.spawn_default().expect("dying");
    let (watcher, reference) = record_peer_watcher(
        &mut table,
        &mut heap,
        dying,
        node,
        (11, 22, 33),
        &[0, 0, 7],
        0x1234,
    );
    assert!(
        log.borrow().is_empty(),
        "an inbound DOP_MONITOR_P is never echoed (`dist.c:2378`)"
    );

    // A reason built on the test heap, so "carried" is provable: the signal names the very term
    // the death was handed, not an equal-looking copy of a `make_small`.
    let reason = heap
        .tuple(&[make_small(41), make_small(42)])
        .expect("the reason fits the test heap");
    assert_eq!(size_object(reason), 3, "header + two elements");

    table
        .terminate(dying, reason)
        .expect("the dying process is live");
    assert!(!table.contains(dying), "the process died");
    assert!(
        table.remote_watchers(dying).is_empty(),
        "the record was drained by the death"
    );

    let signals = log.borrow();
    assert_eq!(
        signals.len(),
        1,
        "exactly one DOP_MONITOR_P_EXIT per recorded watcher, got {signals:?}"
    );
    let (watched, signalled_watcher, signalled_reference, carried) = monitor_exit(&signals[0]);
    assert_eq!(watched, dying, "the **watched** local process, first on the wire");
    assert_eq!(signalled_watcher, watcher, "the recorded remote watcher");
    assert_eq!(
        signalled_reference,
        RemoteWatcherRef::Remote(reference),
        "the peer's own reference goes back verbatim"
    );
    assert_eq!(
        carried, reason,
        "the reason is the very term the death was handed (the router must serialise it \
         synchronously — it may live in the dying heap)"
    );
    assert!(
        compare::eq(carried, reason, &names),
        "the routed reason is structurally the death's reason"
    );
}

#[test]
fn watchers_on_two_different_nodes_each_get_their_own_signal() {
    // The node is part of the watcher's identity, so two peers watching the same local process
    // are two records, two signals and two distinct `DOP_MONITOR_P_EXIT` destinations — never
    // de-duplicated (there is nothing to de-duplicate: the C keys each monitor by its own
    // reference, `erl_proc_sig_queue.c`).
    let (mut table, mut names, node, log) = fixture();
    let other_node = interning(&mut names, "other@host");
    assert_ne!(node, other_node, "two distinct sysname atoms");
    let mut heap = Heap::new(64).expect("test heap");
    let dying = table.spawn_default().expect("dying");

    let (first, first_ref) = record_peer_watcher(
        &mut table,
        &mut heap,
        dying,
        node,
        (1, 2, 3),
        &[1, 1, 1],
        0xAAAA,
    );
    let (second, second_ref) = record_peer_watcher(
        &mut table,
        &mut heap,
        dying,
        other_node,
        (4, 5, 6),
        &[2, 2, 2],
        0xBBBB,
    );
    assert_ne!(first.node(), second.node(), "different peer nodes");
    assert_eq!(table.remote_watchers(dying).len(), 2, "two records");
    assert!(log.borrow().is_empty(), "recording a watcher routes nothing");

    let reason = make_small(5);
    table.terminate(dying, reason).expect("dying is live");

    let signals = log.borrow();
    assert_eq!(signals.len(), 2, "one signal per watcher, got {signals:?}");
    let routed: Vec<(RemotePid, RemoteWatcherRef)> = signals
        .iter()
        .map(|signal| {
            let (watched, watcher, reference, carried) = monitor_exit(signal);
            assert_eq!(watched, dying);
            assert_eq!(carried, reason);
            (watcher, reference)
        })
        .collect();
    assert_eq!(
        routed,
        vec![
            (first, RemoteWatcherRef::Remote(first_ref)),
            (second, RemoteWatcherRef::Remote(second_ref)),
        ],
        "each signal names its own node, its own watcher and its own reference, in record order"
    );
    // ... and each destination is the node that watcher lives on.
    assert_eq!(routed[0].0.node(), node);
    assert_eq!(routed[1].0.node(), other_node);
}

#[test]
fn a_locally_minted_record_routes_its_monitor_ref_intact() {
    // The other reference spelling: a record installed through `monitor_from_remote` (the
    // `MonitorRef` form) routes `RemoteWatcherRef::Local(…)` unchanged, so a router can
    // materialise it with `MonitorRef::to_eterm` exactly as it does for the monitor family.
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let dying = table.spawn_default().expect("dying");
    let watcher =
        RemotePid::from_eterm(remote_pid(&mut heap, node, 21, 22, 23)).expect("an external pid");
    let reference = table.next_reference();
    table
        .monitor_from_remote(dying, watcher, reference, MonitorKind::Process)
        .expect("dying is live");
    assert!(log.borrow().is_empty(), "an inbound DOP_MONITOR_P is never echoed");

    table.terminate(dying, make_small(1)).expect("dying is live");

    let signals = log.borrow();
    assert_eq!(signals.len(), 1, "one signal, got {signals:?}");
    let (watched, signalled_watcher, signalled_reference, reason) = monitor_exit(&signals[0]);
    assert_eq!(watched, dying);
    assert_eq!(signalled_watcher, watcher);
    assert_eq!(
        signalled_reference,
        RemoteWatcherRef::Local(reference),
        "the locally minted reference is carried intact"
    );
    assert_eq!(reason, make_small(1));
}

#[test]
fn only_the_dying_processes_own_watchers_are_routed() {
    // The record table is keyed by the **watched** local process, so a peer watching a
    // different (or still-live) process must never receive a DOWN when this one dies.
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let first = table.spawn_default().expect("first");
    let second = table.spawn_default().expect("second");
    let survivor = table.spawn_default().expect("survivor");

    let (first_watcher, first_ref) = record_peer_watcher(
        &mut table,
        &mut heap,
        first,
        node,
        (31, 32, 33),
        &[3, 1, 1],
        0xC1,
    );
    let (second_watcher, second_ref) = record_peer_watcher(
        &mut table,
        &mut heap,
        second,
        node,
        (41, 42, 43),
        &[3, 2, 2],
        0xC2,
    );
    let (survivor_watcher, _) = record_peer_watcher(
        &mut table,
        &mut heap,
        survivor,
        node,
        (51, 52, 53),
        &[3, 3, 3],
        0xC3,
    );
    assert!(log.borrow().is_empty());

    // 1. The *second* process dies: exactly its own watcher is told.
    let second_reason = make_small(2);
    table
        .terminate(second, second_reason)
        .expect("second is live");
    {
        let signals = log.borrow();
        assert_eq!(signals.len(), 1, "only the dying process's watcher, got {signals:?}");
        let (watched, watcher, reference, reason) = monitor_exit(&signals[0]);
        assert_eq!(watched, second);
        assert_eq!(watcher, second_watcher);
        assert_eq!(reference, RemoteWatcherRef::Remote(second_ref));
        assert_eq!(reason, second_reason);
    }
    // The two other records are untouched.
    assert_eq!(table.remote_watchers(first).len(), 1);
    assert_eq!(
        table.remote_watchers(survivor).len(),
        1,
        "another process's record survives this death"
    );
    assert_eq!(table.remote_watchers(survivor)[0].watcher, survivor_watcher);

    // 2. The *first* process dies too: now exactly its own watcher is told.
    log.borrow_mut().clear();
    let first_reason = make_small(1);
    table.terminate(first, first_reason).expect("first is live");
    {
        let signals = log.borrow();
        assert_eq!(signals.len(), 1, "only the dying process's watcher, got {signals:?}");
        let (watched, watcher, reference, reason) = monitor_exit(&signals[0]);
        assert_eq!(watched, first);
        assert_eq!(watcher, first_watcher);
        assert_eq!(reference, RemoteWatcherRef::Remote(first_ref));
        assert_eq!(reason, first_reason);
    }

    // 3. A still-live watched process routes nothing at all.
    log.borrow_mut().clear();
    assert!(table.contains(survivor), "the survivor is still live");
    assert_eq!(
        table.remote_watchers(survivor).len(),
        1,
        "a live process keeps its incoming-watcher record"
    );
    assert!(
        log.borrow().is_empty(),
        "no death, no DOP_MONITOR_P_EXIT — nothing to report"
    );
}

#[test]
fn a_structured_reason_is_carried_faithfully() {
    // A structured (non-`normal`) reason is a multi-word heap term, so this pins that the
    // signal carries the *term*, not a collapsed atom or a fabrication: `{bad, {1, 2}}` is
    // rebuilt element for element by the router's serialisation (which is why it must happen
    // synchronously — the term lives in the dying process's caller's heap).
    let (mut table, mut names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let dying = table.spawn_default().expect("dying");
    let (watcher, reference) = record_peer_watcher(
        &mut table,
        &mut heap,
        dying,
        node,
        (61, 62, 63),
        &[4, 4, 4],
        0xD4,
    );

    let inner = heap
        .tuple(&[make_small(1), make_small(2)])
        .expect("the inner tuple fits");
    let bad = interning(&mut names, "bad");
    let reason = heap.tuple(&[bad, inner]).expect("the reason fits");
    let normal = interning(&mut names, "normal");
    assert_eq!(
        size_object(reason),
        6,
        "`{{bad, {{1, 2}}}}` is (header + 2 words) + (header + 2 words)"
    );
    assert!(
        !compare::eq(reason, normal, &names),
        "a structured reason is not `normal`"
    );

    table.terminate(dying, reason).expect("dying is live");

    let signals = log.borrow();
    assert_eq!(signals.len(), 1, "one signal, got {signals:?}");
    let (watched, signalled_watcher, signalled_reference, carried) = monitor_exit(&signals[0]);
    assert_eq!(watched, dying);
    assert_eq!(signalled_watcher, watcher);
    assert_eq!(signalled_reference, RemoteWatcherRef::Remote(reference));
    assert_eq!(carried, reason, "the very term, not a look-alike");
    assert!(
        compare::eq(carried, reason, &names),
        "the structured reason is carried structurally intact"
    );
}

#[test]
fn without_a_router_a_death_is_record_only() {
    // With no distribution seam installed the table is record-only: `route` is an `Ok(())` no-op,
    // so a death with an incoming remote watcher still succeeds, still drops the record, and
    // **fabricates no signal** — nothing panics and nothing is invented.
    let (mut table, _names, node) = bare();
    let mut heap = Heap::new(64).expect("test heap");
    let dying = table.spawn_default().expect("dying");
    let other = table.spawn_default().expect("other");
    let (watcher, reference) = record_peer_watcher(
        &mut table,
        &mut heap,
        dying,
        node,
        (71, 72, 73),
        &[5, 5, 5],
        0xE5,
    );
    assert_eq!(table.remote_watchers(dying).len(), 1);
    assert_eq!(
        table.remote_watchers(dying)[0],
        RemoteWatcher {
            reference: RemoteWatcherRef::Remote(reference),
            watcher,
            kind: MonitorKind::Process,
        }
    );

    let reason = heap
        .tuple(&[make_small(9), make_small(8)])
        .expect("the reason fits");
    table
        .terminate(dying, reason)
        .expect("a router-less death still succeeds");

    assert!(!table.contains(dying), "the process died regardless");
    assert!(
        table.remote_watchers(dying).is_empty(),
        "the record was drained, so nothing is left to resolve later"
    );
    // Nothing was fabricated anywhere else either.
    assert!(table.get(other).expect("other is live").mailbox().is_empty());
    assert!(table.drain_woken().is_empty(), "no receiver was woken");
    // ... and the table still works afterwards (the next death is a plain one).
    table
        .terminate(other, make_small(0))
        .expect("termination keeps working");
    assert!(!table.contains(other));
}

#[test]
fn one_signal_per_recorded_watcher_exactly() {
    // Three watchers — two peer-minted references on one node, one locally minted reference —
    // on one dying process: exactly three `MonitorExit` signals, no duplicate, no other
    // `DistSignal` variant, and each reference back in the spelling it was recorded in. This is
    // the "one signal per record, never a broadcast and never a collapse" pin.
    let (mut table, _names, node, log) = fixture();
    let mut heap = Heap::new(64).expect("test heap");
    let dying = table.spawn_default().expect("dying");

    let (first, first_ref) = record_peer_watcher(
        &mut table,
        &mut heap,
        dying,
        node,
        (81, 82, 83),
        &[6, 1, 1],
        0xF1,
    );
    let (second, second_ref) = record_peer_watcher(
        &mut table,
        &mut heap,
        dying,
        node,
        (91, 92, 93),
        &[6, 2, 2],
        0xF2,
    );
    let local_watcher =
        RemotePid::from_eterm(remote_pid(&mut heap, node, 101, 102, 103)).expect("an ext pid");
    let local_ref = table.next_reference();
    table
        .monitor_from_remote(dying, local_watcher, local_ref, MonitorKind::Port)
        .expect("dying is live");
    assert_eq!(table.remote_watchers(dying).len(), 3, "three records");
    assert!(log.borrow().is_empty());

    let reason = make_small(3);
    table.terminate(dying, reason).expect("dying is live");

    let signals = log.borrow();
    assert_eq!(
        signals.len(),
        3,
        "exactly one signal per recorded watcher, got {signals:?}"
    );
    let routed: Vec<(RemotePid, RemoteWatcherRef, Eterm)> = signals
        .iter()
        .map(|signal| {
            let (watched, watcher, reference, carried) = monitor_exit(signal);
            assert_eq!(watched, dying, "every signal names the watched process");
            assert_eq!(carried, reason, "every signal carries the death's reason");
            (watcher, reference, carried)
        })
        .collect();
    // No duplicates: pairwise distinct destinations, and the record spellings survive.
    for (i, left) in routed.iter().enumerate() {
        for right in routed.iter().skip(i + 1) {
            assert_ne!(left.0, right.0, "one signal per watcher, never two for one");
            assert_ne!(left.1, right.1, "one signal per reference, never two for one");
        }
    }
    assert_eq!(
        routed
            .iter()
            .map(|(watcher, reference, _)| (*watcher, *reference))
            .collect::<Vec<_>>(),
        vec![
            (first, RemoteWatcherRef::Remote(first_ref)),
            (second, RemoteWatcherRef::Remote(second_ref)),
            (local_watcher, RemoteWatcherRef::Local(local_ref)),
        ],
        "record order, each reference in the spelling it was recorded in"
    );
    assert!(
        table.remote_watchers(dying).is_empty(),
        "the records are gone with the process (nothing left to resolve twice)"
    );
}