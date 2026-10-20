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

//! [`ProcessTable::node_down`] — the whole **connection-down** semantics of one lost remote node.
//!
//! This is the Rust analogue of the C's connection-down state machine
//! (`erts/emulator/beam/dist.c:379-423`, `con_monitor_link_seq_cleanup`), which walks the dead
//! node's own `ErtsMonLnkDist` tables with `foreach_delete` iterators and deletes **every** entry —
//! in both directions: the records *to* the dead node are fired as a DOWN/EXIT first
//! (`monitor_connection_down`, `dist.c:304-324`; `link_connection_down`, `dist.c:341-345`), and the
//! records *from* it are dropped, because nothing can be resolved, demonitored or delivered to a
//! node that is gone.
//!
//! The cases:
//!
//! 1. [`a_trapping_link_partner_traps_the_node_down_exit`] — a trapping peer survives with
//!    `{'EXIT', RemotePid, noconnection}`, the external pid in the body and the node atom as the
//!    message `from` (`erts_proc_sig_send_link_exit_noconnection`,
//!    `erl_proc_sig_queue.c:2553-2576`: `from_tag` = the node name, `from_item` = the external pid,
//!    op = `ERTS_SIG_Q_OP_EXIT_LINKED`, reason = `am_noconnection`);
//! 2. [`a_non_trapping_link_partner_dies_with_exactly_noconnection`] — a non-trapping peer
//!    terminates with the reason **exactly** `noconnection` (the link family never rewrites it to
//!    `killed`, `erl_proc_sig_queue.c:4740`/`:4768`);
//! 3. [`a_remote_monitor_down_is_delivered_and_wakes_a_waiting_watcher`] — a `Waiting` watcher
//!    gets `{'DOWN', Ref, process, RemotePid, noconnection}` and lands in the woken list;
//! 4. [`a_node_down_purges_only_the_lost_nodes_records_in_both_directions`] — the incoming watcher
//!    records (`remote_watchers`) of the lost node are purged while another node's **survive**, and
//!    so are the outgoing `remote_links`/`remote_monitors` records (they fire *and* go).
//!
//! **No `erl` oracle probe is possible for these**: observing an inbound remote watcher record, or
//! a link's `noconnection` exit, from a single `erl -noshell -eval` run needs a second live node
//! (and a `net_kernel` connection to tear down), which this crate's tests deliberately do not
//! require. The ground truth is the C cited above, and each case is written as the C behaves.
//!
//! Determinism: no sleep, no wall-clock bound, no external process.

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_common::Result;
use beam_process::prelude::*;
use beam_term::prelude::{
    boxed_val, external_pid_size_words, external_ref_size_words, make_atom, signed_value,
    tuple_element, tuple_size, write_external_pid, write_external_ref, AtomTable, Eterm,
};

/// A table, the atom table its [`SignalAtoms`] were interned from, and the sysname atom term
/// standing in for the remote node.
fn fixture() -> (ProcessTable, AtomTable, Eterm) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    let node = make_atom(names.intern("remote@host"));
    names.intern("elsewhere@host");
    names.intern("noconnection");
    (ProcessTable::new(atoms), names, node)
}

/// The term for a name the fixture interned.
fn named(names: &AtomTable, name: &str) -> Eterm {
    make_atom(names.lookup(name).expect("the fixture interned it"))
}

/// The heap a decoded external pid/reference is built on (only the *term* is a heap object; the
/// [`RemotePid`]/[`RemoteRef`] that decompose it are plain values).
fn wire_heap() -> Heap {
    Heap::new(64).expect("the wire heap allocates")
}

/// Builds an external pid term on `heap` (the frozen 4-word `[header, node, creation, data]`
/// thing).
fn remote_pid(heap: &mut Heap, node: Eterm, number: u32, serial: u32, creation: u32) -> Eterm {
    // SAFETY: `heap` hands out `external_pid_size_words()` (4) exclusively-owned, writable words.
    let p = unsafe {
        heap.alloc_words(external_pid_size_words())
            .expect("the heap has room")
    };
    // SAFETY: `p` addresses the four words just handed out and `write_external_pid` writes all of
    // them before the pointer escapes; `node` is a live atom term.
    unsafe { write_external_pid(p, node, number, serial, creation) }
}

/// Builds an external reference term on `heap` — the shape a peer's `DOP_MONITOR_P` `Ref` arrives
/// as.
fn remote_ref(heap: &mut Heap, node: Eterm, ids: &[u32], creation: u32) -> Eterm {
    // SAFETY: `heap` hands out `external_ref_size_words(ids.len())` exclusively-owned words (and
    // `ids.len() <= MAX_REF_IDS` at every call site here).
    let p = unsafe {
        heap.alloc_words(external_ref_size_words(ids.len()))
            .expect("the heap has room")
    };
    // SAFETY: `p` addresses those words and `write_external_ref` writes all of them before the
    // pointer escapes; `node` is a live atom term.
    unsafe { write_external_ref(p, node, ids, creation) }
}

/// The payload of `pid`'s single queued message.
fn sole_payload(table: &ProcessTable, pid: ProcessId) -> Eterm {
    let mailbox = table.get(pid).expect("the recipient is live").mailbox();
    assert_eq!(mailbox.len(), 1, "exactly one message is queued");
    mailbox.peek().expect("one message is queued").payload()
}

/// A process-death observer recording `(pid, reason)` for every death — how a termination reason is
/// read without a linked peer. Only an **immediate** reason may be retained (this file's reason is
/// the `noconnection` atom).
struct DeathLog {
    deaths: Rc<RefCell<Vec<(ProcessId, Eterm)>>>,
}

impl ProcessExitObserver for DeathLog {
    fn process_exited(&mut self, exit: &ProcessExit<'_>) {
        self.deaths.borrow_mut().push((exit.pid(), exit.reason()));
    }
}

// --- the fired signals ------------------------------------------------------

#[test]
fn a_trapping_link_partner_traps_the_node_down_exit() -> Result<()> {
    let (mut table, names, node) = fixture();
    let mut wire = wire_heap();
    let peer = table.spawn_default()?;
    table.get_mut(peer).expect("peer").set_trap_exit(true);
    let remote = RemotePid::from_eterm(remote_pid(&mut wire, node, 4, 5, 6))?;
    table.record_remote_link(peer, remote)?;
    let reason = named(&names, "noconnection");

    table.node_down(node, reason)?;

    assert!(table.contains(peer), "a trapping peer survives the node down");
    assert!(
        table.remote_links(peer).is_empty(),
        "the link record to the lost node is gone"
    );
    let message = table
        .get(peer)
        .expect("peer is live")
        .mailbox()
        .peek()
        .expect("one message is queued");
    assert_eq!(
        message.from(),
        node,
        "the `from` slot is the node atom (`from_tag`, `erl_proc_sig_queue.c:2568`)"
    );
    let payload = message.payload();
    // SAFETY: `payload` is the live `{'EXIT', RemotePid, Reason}` tuple on the peer's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), named(&names, "EXIT"));
        assert_eq!(
            RemotePid::from_eterm(tuple_element(payload, 1))?,
            remote,
            "the external pid is in the message body (`from_item`)"
        );
        assert_eq!(
            tuple_element(payload, 2),
            reason,
            "the reason is `noconnection`, never `killed`"
        );
    }
    Ok(())
}

#[test]
fn a_non_trapping_link_partner_dies_with_exactly_noconnection() -> Result<()> {
    let (mut table, names, node) = fixture();
    let mut wire = wire_heap();
    let peer = table.spawn_default()?;
    let remote = RemotePid::from_eterm(remote_pid(&mut wire, node, 7, 8, 9))?;
    table.record_remote_link(peer, remote)?;
    let deaths = Rc::new(RefCell::new(Vec::new()));
    table.set_process_exit_observer(Box::new(DeathLog {
        deaths: Rc::clone(&deaths),
    }));
    let reason = named(&names, "noconnection");

    table.node_down(node, reason)?;

    assert!(!table.contains(peer), "a non-trapping peer follows the node down");
    assert_eq!(
        deaths.borrow().as_slice(),
        &[(peer, reason)],
        "the reason is exactly `noconnection` — the link family never rewrites it to `killed` \
         (`erl_proc_sig_queue.c:2571-2573`, `:4740`/`:4768`)"
    );
    Ok(())
}

#[test]
fn a_remote_monitor_down_is_delivered_and_wakes_a_waiting_watcher() -> Result<()> {
    let (mut table, names, node) = fixture();
    let mut wire = wire_heap();
    let watcher = table.spawn_default()?;
    let ext = remote_pid(&mut wire, node, 11, 12, 13);
    let remote = RemotePid::from_eterm(ext)?;
    let reference = table.monitor_term(watcher, ext, MonitorKind::Process)?;
    table
        .get_mut(watcher)
        .expect("watcher")
        .set_state(ProcessState::Waiting);
    let reason = named(&names, "noconnection");

    table.node_down(node, reason)?;

    assert_eq!(
        table.drain_woken(),
        vec![watcher],
        "the blocked watcher is woken by the DOWN"
    );
    assert!(
        table.remote_monitors(watcher).is_empty(),
        "the fired monitor record is gone"
    );
    let payload = sole_payload(&table, watcher);
    // SAFETY: `payload` is the live `{'DOWN', Ref, process, RemotePid, Reason}` tuple on the
    // watcher's heap; the five element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), named(&names, "DOWN"));
        assert_eq!(tuple_element(payload, 2), named(&names, "process"));
        assert_eq!(
            RemotePid::from_eterm(tuple_element(payload, 3))?,
            remote,
            "the external pid target is in the body"
        );
        assert_eq!(tuple_element(payload, 4), reason);
        // Element 1 is the reference thing; its last payload word encodes the reference id.
        let words = boxed_val(tuple_element(payload, 1));
        assert_eq!(signed_value(*words.add(3)), reference.id() as i64);
    }
    Ok(())
}

// --- the purge --------------------------------------------------------------

#[test]
fn a_node_down_purges_only_the_lost_nodes_records_in_both_directions() -> Result<()> {
    let (mut table, names, node) = fixture();
    let mut wire = wire_heap();
    let other_node = named(&names, "elsewhere@host");

    // The watched local process: one watcher on the lost node, one on a live node.
    let watched = table.spawn_default()?;
    let doomed_watcher = RemotePid::from_eterm(remote_pid(&mut wire, node, 21, 22, 23))?;
    let survivor_watcher = RemotePid::from_eterm(remote_pid(&mut wire, other_node, 24, 25, 26))?;
    let local_ref = table.next_reference();
    table.monitor_from_remote(watched, doomed_watcher, local_ref, MonitorKind::Process)?;
    let peer_ref = RemoteRef::from_eterm(remote_ref(&mut wire, other_node, &[1, 2, 3], 9))?;
    table.monitor_from_remote_ref(
        watched,
        survivor_watcher,
        peer_ref,
        MonitorKind::Process,
    )?;
    assert_eq!(table.remote_watchers(watched).len(), 2, "both records exist");

    // A local peer with one link per node, and one outgoing monitor per node.
    let peer = table.spawn_default()?;
    table.get_mut(peer).expect("peer").set_trap_exit(true);
    let doomed_link = RemotePid::from_eterm(remote_pid(&mut wire, node, 31, 32, 33))?;
    let survivor_link = RemotePid::from_eterm(remote_pid(&mut wire, other_node, 34, 35, 36))?;
    table.record_remote_link(peer, doomed_link)?;
    table.record_remote_link(peer, survivor_link)?;
    let other_ext = remote_pid(&mut wire, other_node, 41, 42, 43);
    let survivor_monitor = table.monitor_term(peer, other_ext, MonitorKind::Process)?;

    table.node_down(node, named(&names, "noconnection"))?;

    // The incoming watcher record of the lost node is gone; the other node's survives.
    assert_eq!(
        table.remote_watchers(watched),
        &[RemoteWatcher {
            reference: RemoteWatcherRef::Remote(peer_ref),
            watcher: survivor_watcher,
            kind: MonitorKind::Process,
        }],
        "a watcher record from the dead node can never be resolved nor demonitored, so it is \
         purged; another node's record is untouched"
    );
    // The outgoing link record of the lost node is gone; the other node's survives.
    assert_eq!(
        table.remote_links(peer),
        &[survivor_link],
        "the link to the lost node fires and goes; another node's stays"
    );
    // ... and so is the outgoing monitor record (the survivor's is `remote_monitors`' only entry).
    assert_eq!(
        table.remote_monitors(peer),
        &[RemoteMonitor {
            reference: survivor_monitor,
            remote: RemotePid::from_eterm(other_ext)?,
            kind: MonitorKind::Process,
        }],
        "the monitor on the lost node fired and went; another node's stays"
    );
    // The trapping peer received exactly one message — the link exit, not a second DOWN.
    assert!(table.contains(peer));
    let payload = sole_payload(&table, peer);
    // SAFETY: `payload` is the live `{'EXIT', RemotePid, Reason}` tuple on the peer's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(
            RemotePid::from_eterm(tuple_element(payload, 1))?,
            doomed_link,
            "the exit is about the lost node's partner"
        );
    }

    // A re-connect must not resurrect the purged record: nothing mentions the lost node any more.
    table.node_down(node, named(&names, "noconnection"))?;
    assert_eq!(
        table.remote_watchers(watched).len(),
        1,
        "a second node-down sweep finds nothing of the lost node to purge"
    );
    Ok(())
}
