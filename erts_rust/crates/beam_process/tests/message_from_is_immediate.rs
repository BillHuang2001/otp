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

//! Regression evidence that [`Message::from`](beam_message::Message::from) is **always an
//! immediate** on every signal-message path this crate produces.
//!
//! `from` is the runtime's `ERL_MESSAGE_FROM(mp)` (`mp->m[2]`), and the C requires it to be an
//! immediate — `ASSERT(is_immed(ERL_MESSAGE_FROM(msg)))` (`erl_proc_sig_queue.c:8441`). It is
//! **not** part of the process GC root set ([`Process::collect`] gathers only each queued
//! message's *payload*), so a boxed term stored there is **never relocated**: the first
//! collection of the owning process turns it into a dangling pointer into the evacuated
//! from-space. Production reads it (`beam_rt`'s `crash_dump.rs` renders `message.from()`), so a
//! stale word is an observable use-after-free.
//!
//! The four producers that used a heap term — the `{Name, Node}` pair of a named monitor, and
//! the heap-materialised external pid of a remote DOWN/EXIT — store an **immediate** now; the
//! `{Name, Node}` pair and the external pid stay in the message **body**, exactly as the C keeps
//! them. One test per producer — eight producer cases plus the collection-survival evidence:
//!
//! 1. [`a_named_monitor_down_reports_the_resolved_pid_as_from`] — the successful named monitor's
//!    `from` is the resolved internal pid (`target.to_eterm()`), the body still carries
//!    `{Name, Node}`.
//! 2. [`an_ordinary_monitor_down_reports_the_target_pid_as_from`] — the non-named branch of the
//!    same builder keeps its immediate pid `from`.
//! 3. [`a_named_monitor_on_an_unregistered_name_reports_the_system_atom_as_from`] — a monitor
//!    that failed **by name** (the C's `am_system`, `erl_proc_sig_queue.c:4991`).
//! 4. [`a_remote_down_reports_the_node_atom_as_from`] — the C's node atom for a
//!    `DIST_PROC`/`DIST_PORT` monitor (`erl_proc_sig_queue.c:4984`/`:5014`).
//! 5. [`a_remote_exit_reports_the_node_atom_as_from`] — the same for a node-down exit.
//! 6. [`a_named_monitor_down_from_survives_a_collection`] — the DOWN is queued, the nursery is
//!    filled **exactly** with unrooted garbage, the owning process collects, and `from` is still
//!    the same immediate while the relocated payload still carries `{Name, Node}`.
//! 7. [`a_noproc_monitor_down_reports_the_dead_target_pid_as_from`] — the bare-`noproc` DOWN of a
//!    monitor on a non-live pid (`deliver_noproc_down`).
//! 8. [`a_port_monitor_down_reports_the_port_as_from`] — the port-monitor DOWN
//!    (`enqueue_port_down_message`).
//! 9. [`a_port_link_exit_reports_the_port_as_from`] — the port link exit
//!    (`enqueue_port_exit_message`).
//!
//! Pre-fix, tests 1, 3, 4, 5 and 6 fail: `from` was the boxed `{Name, Node}` tuple / external
//! pid thing, so `is_immediate(from)` is false (and the returned term is not the pid/node atom).
//! Tests 2 and 7-9 pin producers that were already correct but **unpinned** (the port and
//! bare-`noproc` builders pass a port/pid immediate straight through), so they pass either way —
//! they close a coverage gap in the crate docs' "one test per producer" claim.

use beam_alloc::Heap;
use beam_common::Result;
use beam_process::prelude::*;
use beam_term::prelude::{
    external_pid_size_words, is_atom, is_immediate, make_atom, make_small, tuple_element,
    tuple_size, write_external_pid, AtomTable, Eterm,
};

/// A process table plus the atom table its [`SignalAtoms`] were interned from, plus the sysname
/// atom term standing in for the remote node.
fn fixture() -> (ProcessTable, AtomTable, Eterm) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    let node = make_atom(names.intern("remote@host"));
    (ProcessTable::new(atoms), names, node)
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

/// The `(from, payload)` of `pid`'s single queued message (panics unless exactly one is queued).
fn sole_message(table: &ProcessTable, pid: ProcessId) -> (Eterm, Eterm) {
    let mailbox = table.get(pid).expect("the recipient is live").mailbox();
    assert_eq!(mailbox.len(), 1, "exactly one message is queued");
    let message = mailbox.peek().expect("one message is queued");
    (message.from(), message.payload())
}

/// The `{Name, Node}` pair the DOWN body carries — asserting the *body* still holds the named
/// descriptor (only the `from` slot changed).
///
/// # Safety
///
/// The caller must pass a live 5-tuple `payload` whose element 3 is the `{Name, Node}` pair, both
/// written on a live heap.
unsafe fn named_body_target(payload: Eterm) -> Eterm {
    // SAFETY: the caller promises a live 5-tuple `payload` whose element 3 is the live
    // `{Name, Node}` pair the table wrote on a live heap, so slot 3 is a live tagged term and the
    // pair's two slots (0/1) are in bounds.
    unsafe {
        let target = tuple_element(payload, 3);
        assert_eq!(
            tuple_size(target),
            2,
            "the body carries the `{{Name, Node}}` pair"
        );
        target
    }
}
/// Fills `process`'s nursery **exactly** with unrooted garbage, so the next collection sees a
/// full round and relocates every live object (the `tests/exhaustion.rs` technique).
fn fill_remaining_nursery(process: &mut Process) {
    let available = process.collector().young().available_words();
    assert!(available > 0, "there is room left to fill");
    // A tuple of `available - 1` elements occupies exactly `available` words (arityval + elems).
    let filler = vec![make_small(0); available - 1];
    process
        .heap_mut()
        .tuple(&filler)
        .expect("the nursery fills exactly");
    assert_eq!(process.collector().young().available_words(), 0);
}

// ---------------------------------------------------------------------------
// Producer 1: the named-monitor DOWN (`deliver_down_message`, site 2)
// ---------------------------------------------------------------------------

#[test]
fn a_named_monitor_down_reports_the_resolved_pid_as_from() -> Result<()> {
    let (mut table, mut names, _node) = fixture();
    let down = names.intern("DOWN");
    let process_atom = names.intern("process");
    let name = names.intern("server");
    let node = names.intern("nonode@nohost");

    let watcher = table.spawn_default()?;
    let target = table.spawn_default()?;
    table.register(name, target)?;
    table.monitor_named(watcher, name, node, MonitorKind::Process)?;

    let reason = make_small(7);
    table.terminate(target, reason)?;

    let (from, payload) = sole_message(&table, watcher);
    assert!(
        is_immediate(from),
        "`from` must be an immediate (the C asserts is_immed(ERL_MESSAGE_FROM))"
    );
    assert_eq!(
        from,
        target.to_eterm(),
        "`from` is the resolved internal pid, never the {{Name, Node}} tuple"
    );
    // SAFETY: `payload` is the live 5-tuple the table built on the watcher's heap; the five
    // element slots are in bounds and element 3 is the live `{Name, Node}` pair.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), make_atom(down));
        assert_eq!(tuple_element(payload, 2), make_atom(process_atom));
        let body_target = named_body_target(payload);
        assert_eq!(tuple_element(body_target, 0), make_atom(name));
        assert_eq!(tuple_element(body_target, 1), make_atom(node));
        assert_eq!(tuple_element(payload, 4), reason);
    }
    Ok(())
}

#[test]
fn an_ordinary_monitor_down_reports_the_target_pid_as_from() -> Result<()> {
    let (mut table, mut names, _node) = fixture();
    let down = names.intern("DOWN");
    let process_atom = names.intern("process");

    let watcher = table.spawn_default()?;
    let target = table.spawn_default()?;
    table.monitor(watcher, target, MonitorKind::Process)?;

    let reason = make_small(3);
    table.terminate(target, reason)?;

    let (from, payload) = sole_message(&table, watcher);
    assert!(is_immediate(from));
    assert_eq!(from, target.to_eterm(), "`from` is the target pid immediate");
    // SAFETY: `payload` is the live 5-tuple the table built on the watcher's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), make_atom(down));
        assert_eq!(tuple_element(payload, 2), make_atom(process_atom));
        assert_eq!(tuple_element(payload, 3), target.to_eterm());
        assert_eq!(tuple_element(payload, 4), reason);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Producer 2: a monitor that failed by name (`deliver_named_noproc_down`, site 1)
// ---------------------------------------------------------------------------

#[test]
fn a_named_monitor_on_an_unregistered_name_reports_the_system_atom_as_from() -> Result<()> {
    let (mut table, mut names, _node) = fixture();
    let name = names.intern("nobody");
    let node = names.intern("nonode@nohost");
    let system = names.intern("system");

    let watcher = table.spawn_default()?;
    table.monitor_named(watcher, name, node, MonitorKind::Process)?;

    let (from, payload) = sole_message(&table, watcher);
    assert!(is_immediate(from), "`from` must be an immediate");
    assert!(is_atom(from), "a failed-by-name monitor's `from` is an atom");
    assert_eq!(
        from,
        make_atom(system),
        "the C stores `am_system` for a monitor that failed by name"
    );
    // The `{Name, Node}` pair still lives in the body, not in `from`.
    // SAFETY: `payload` is the live 5-tuple the table built on the watcher's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        let body_target = named_body_target(payload);
        assert_eq!(tuple_element(body_target, 0), make_atom(name));
        assert_eq!(tuple_element(body_target, 1), make_atom(node));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Producers 3-4: the remote DOWN / EXIT (`node_down`)
// ---------------------------------------------------------------------------

#[test]
fn a_remote_down_reports_the_node_atom_as_from() -> Result<()> {
    let (mut table, mut names, node) = fixture();
    let mut heap = Heap::new(64)?;
    let watcher = table.spawn_default()?;
    let ext = remote_pid(&mut heap, node, 7, 8, 9);
    let rpid = RemotePid::from_eterm(ext)?;
    table.monitor_term(watcher, ext, MonitorKind::Process)?;

    let reason = make_atom(names.intern("noconnection"));
    table.node_down(node, reason)?;

    let (from, payload) = sole_message(&table, watcher);
    assert!(is_immediate(from), "`from` must be an immediate");
    assert_eq!(
        from, node,
        "the C stores the node atom for a DIST_PROC/DIST_PORT monitor, not the external pid"
    );
    // The heap-materialised external pid stays in the message body.
    // SAFETY: `payload` is the live 5-tuple the table built on the watcher's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(
            RemotePid::from_eterm(tuple_element(payload, 3))?,
            rpid,
            "the body still carries the external pid target"
        );
    }
    Ok(())
}

#[test]
fn a_remote_exit_reports_the_node_atom_as_from() -> Result<()> {
    let (mut table, mut names, node) = fixture();
    let mut heap = Heap::new(64)?;
    let peer = table.spawn_default()?;
    table.get_mut(peer).expect("the peer is live").set_trap_exit(true);    let ext = remote_pid(&mut heap, node, 12, 13, 14);
    let rpid = RemotePid::from_eterm(ext)?;
    table.link_term(peer, ext)?;

    let reason = make_atom(names.intern("noconnection"));
    table.node_down(node, reason)?;

    let (from, payload) = sole_message(&table, peer);
    assert!(is_immediate(from), "`from` must be an immediate");
    assert_eq!(from, node, "the node atom, not the external link partner");
    // The heap-materialised external pid stays in the message body.
    // SAFETY: `payload` is the live 3-tuple the table built on the peer's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(
            RemotePid::from_eterm(tuple_element(payload, 1))?,
            rpid,
            "the body still carries the external pid link partner"
        );
        assert_eq!(tuple_element(payload, 2), reason);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Producer 5: the bare-`noproc` DOWN of a monitor on a non-live pid
// (`deliver_noproc_down`)
// ---------------------------------------------------------------------------

/// `monitor/2` on a pid that is already gone delivers `{'DOWN', Ref, process, DeadPid, noproc}`
/// straight into the watcher's mailbox; its `from` is the **dead target's pid immediate**.
///
/// This producer was already correct (it passes its `target` word straight to
/// `build_signal_message`), but it was unpinned — the crate docs claimed every `from` producer was
/// covered here.
#[test]
fn a_noproc_monitor_down_reports_the_dead_target_pid_as_from() -> Result<()> {
    let (mut table, mut names, _node) = fixture();
    let down = names.intern("DOWN");
    let process_atom = names.intern("process");
    let noproc = names.intern("noproc");

    let watcher = table.spawn_default()?;
    // A pid that is no longer live: allocation hands the number back with a bumped serial, so
    // the term is still a valid pid while `contains` is false.
    let dead = {
        let dying = table.spawn_default()?;
        table.remove(dying);
        dying
    };
    assert!(!table.contains(dead));

    let reference = table.monitor(watcher, dead, MonitorKind::Process)?;

    let (from, payload) = sole_message(&table, watcher);
    assert!(
        is_immediate(from),
        "`from` must be an immediate (the C asserts is_immed(ERL_MESSAGE_FROM))"
    );
    assert_eq!(
        from,
        dead.to_eterm(),
        "`from` is the dead target's pid immediate"
    );
    // SAFETY: `payload` is the live 5-tuple the table built on the watcher's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), make_atom(down));
        assert_eq!(
            MonitorRef::from_eterm(tuple_element(payload, 1))?,
            reference,
            "the reference slot still carries the monitor's reference"
        );
        assert_eq!(tuple_element(payload, 2), make_atom(process_atom));
        assert_eq!(tuple_element(payload, 3), dead.to_eterm());
        assert_eq!(tuple_element(payload, 4), make_atom(noproc));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Producers 6-7: the port signal builders (`enqueue_port_down_message`,
// `enqueue_port_exit_message`)
// ---------------------------------------------------------------------------

/// The port-monitor DOWN `{'DOWN', Ref, port, Port, Reason}` — built by
/// `ProcessTable::enqueue_port_down_message` on `port_closed` — carries the **port immediate** in
/// `from`.
#[test]
fn a_port_monitor_down_reports_the_port_as_from() -> Result<()> {
    let (mut table, mut names, _node) = fixture();
    let down = names.intern("DOWN");
    let port_atom = names.intern("port");

    let watcher = table.spawn_default()?;
    let owner = table.spawn_default()?;
    let mut ports = PortTable::new();
    let port = ports.alloc_owned(owner)?;
    table.monitor_port(watcher, port, &mut ports)?;

    let reason = make_small(17);
    table.port_closed(&mut ports, port, reason)?;

    let (from, payload) = sole_message(&table, watcher);
    assert!(is_immediate(from), "`from` must be an immediate");
    assert_eq!(from, port.to_eterm(), "`from` is the port immediate");
    // SAFETY: `payload` is the live 5-tuple the table built on the watcher's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), make_atom(down));
        assert_eq!(tuple_element(payload, 2), make_atom(port_atom));
        assert_eq!(tuple_element(payload, 3), port.to_eterm());
        assert_eq!(tuple_element(payload, 4), reason);
    }
    Ok(())
}

/// The port link exit `{'EXIT', Port, Reason}` — built by
/// `ProcessTable::enqueue_port_exit_message` for a **trapping** linked process on `port_closed` —
/// carries the **port immediate** in `from`.
#[test]
fn a_port_link_exit_reports_the_port_as_from() -> Result<()> {
    let (mut table, mut names, _node) = fixture();
    let exit = names.intern("EXIT");

    let watcher = table.spawn_default()?;
    let owner = table.spawn_default()?;
    table
        .get_mut(watcher)
        .expect("the watcher is live")
        .set_trap_exit(true);
    let mut ports = PortTable::new();
    let port = ports.alloc_owned(owner)?;
    assert!(ports.link(port, watcher)?, "the link is new");

    let reason = make_small(21);
    table.port_closed(&mut ports, port, reason)?;

    let (from, payload) = sole_message(&table, watcher);
    assert!(is_immediate(from), "`from` must be an immediate");
    assert_eq!(from, port.to_eterm(), "`from` is the port immediate");
    // SAFETY: `payload` is the live 3-tuple the table built on the watcher's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), make_atom(exit));
        assert_eq!(tuple_element(payload, 1), port.to_eterm());
        assert_eq!(tuple_element(payload, 2), reason);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Survival across a collection
// ---------------------------------------------------------------------------

/// The named-monitor DOWN is queued, the owning process collects, and the message's `from` is
/// still the same **immediate** — while the relocated payload still carries `{Name, Node}`.
///
/// `from` is not a GC root, so a boxed `from` would be a stale pointer into the evacuated
/// from-space after the collection; the payload *is* a root and is relocated correctly.
#[test]
fn a_named_monitor_down_from_survives_a_collection() -> Result<()> {
    let (mut table, mut names, _node) = fixture();
    let name = names.intern("server");
    let node = names.intern("nonode@nohost");

    let watcher = table.spawn_default()?;
    let target = table.spawn_default()?;
    table.register(name, target)?;
    table.monitor_named(watcher, name, node, MonitorKind::Process)?;
    table.terminate(target, make_small(7))?;

    let (before_from, _before_payload) = sole_message(&table, watcher);
    assert_eq!(before_from, target.to_eterm());

    // Fill the nursery exactly and collect, so every live object is evacuated and relocated.
    let process = table.get_mut(watcher).expect("the watcher is live");    fill_remaining_nursery(process);
    let minor_gcs_before = process.collector().minor_gcs();
    process.collect()?;
    assert_eq!(
        process.collector().minor_gcs(),
        minor_gcs_before + 1,
        "a collection really ran"
    );

    let (from, payload) = sole_message(&table, watcher);
    assert!(
        is_immediate(from),
        "`from` is still an immediate after the collection (a boxed one would be stale)"
    );
    assert_eq!(
        from, before_from,
        "`from` is unchanged by the collection: it is the resolved pid immediate"
    );
    assert_eq!(from, target.to_eterm());
    // The payload was relocated as a root; its `{Name, Node}` body target is intact.
    // SAFETY: `payload` is the live 5-tuple rebuilt on the watcher's new semispace.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        let body_target = named_body_target(payload);
        assert_eq!(tuple_element(body_target, 0), make_atom(name));
        assert_eq!(tuple_element(body_target, 1), make_atom(node));
    }
    Ok(())
}
