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

//! The **collection-decline path of the six `build_signal_message`-only signal builders**.
//!
//! `src/ptab.rs` funnels six control-message sites through the shared
//! [`build_signal_message`](beam_process::ProcessTable) policy — and *only* through it, so a
//! receiver whose relocation must be declined reaches their heap-fragment fallback (via
//! `enqueue_fragment_signal`) instead of a collect-and-grow:
//!
//! | Builder | Trigger (public entry point) | Message |
//! |---|---|---|
//! | `deliver_noproc_down` | `monitor(process, P)` on a dead/never-existing pid | `{'DOWN', Ref, process, P, noproc}` |
//! | `deliver_named_noproc_down` | `monitor_named(process, Name, Node)` on an unregistered name | `{'DOWN', Ref, process, {Name, Node}, noproc}` |
//! | `enqueue_remote_down_message` | `node_down(Node, Reason)` for a recorded remote monitor | `{'DOWN', Ref, process, RemotePid, Reason}` |
//! | `enqueue_remote_exit_message` | `node_down(Node, Reason)` for a **trapping** remote link | `{'EXIT', RemotePid, Reason}` |
//! | `enqueue_port_down_message` | `port_closed(Port, Reason)` for a port monitor | `{'DOWN', Ref, port, Port, Reason}` |
//! | `enqueue_port_exit_message` | `port_closed(Port, Reason)` for a **trapping** port link | `{'EXIT', Port, Reason}` |
//!
//! The other refusal sites (the apply-time `deliver_head_signal`, the enqueue-time
//! `copy_reason_onto` and `ProcessTable::deliver`) are already covered by
//! `tests/signal_fragment_fallback.rs`, `tests/decline_gate.rs` and the `src/ptab.rs` unit tests;
//! these six were the remaining gap recorded in the crate `CONTEXT.md`'s Known Issues. Every
//! builder consults **both** valves — the crate-owned
//! [`Process::has_unpublished_roots`](beam_process::Process) (here: a **relocation-exempt**
//! receiver) and the host's [`CollectionGate`] — so each builder gets a case for each valve, plus
//! one shared **control** proving the identical trigger *would* collect-and-grow with no decline in
//! force (so the fragment route is an *additional* decline, never a replacement).
//!
//! Each case saturates the receiver's nursery to three free words — one short of the four-word
//! first allocation of *every* one of the six builds (a boxed monitor reference for the DOWNs, the
//! `{'EXIT', …}` tuple itself for the EXITs; the remote EXIT's lead allocation is the four-word
//! external-pid thing) — so the relocating arm *would* be taken, and then asserts the delivered
//! message is **fragment-resident** while the receiver's nursery did not move: `minor_gcs`, the
//! young space's `base`/`used_words` and the mature generation's used words are all unchanged,
//! exactly in the shape of `tests/decline_gate.rs`.
//!
//! Determinism: no sleep, no wall-clock bound and no oracle. Every size is derived from the public
//! heap-policy arithmetic and every "did nothing" claim is checked against the receiver's own
//! nursery observables, so the refusal is exact and forced by capacity, never by timing.

use beam_alloc::Heap;
use beam_common::Result;
use beam_gc::HeapPolicy;
use beam_message::Message;
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{
    external_pid_size_words, make_atom, make_small, tuple_element, tuple_size, write_external_pid,
    AtomTable, Eterm,
};

/// The words the four-word tier every one of the six builds allocates **first** occupies: `1`
/// header plus three payload words — a boxed monitor reference for the DOWN builders, the
/// `{'EXIT', …}` three-tuple for the EXIT builders.
const HEAD_WORDS: usize = 4;

/// The free words the receiver's nursery is drained to: the head build's first allocation minus
/// one, so the build is refused **by capacity** at its very first allocation and therefore writes
/// nothing (a request that does not fit is rejected before any word is handed out).
const FREE_WORDS: usize = HEAD_WORDS - 1;

/// The receiver's nursery in words.
const NURSERY_WORDS: usize = HEAD_WORDS + FREE_WORDS;

/// The filler message's size in words: `1` header plus three immediate elements — exactly the
/// words the drain occupies, leaving [`FREE_WORDS`] free.
const FILLER_WORDS: usize = NURSERY_WORDS - FREE_WORDS;

/// A host gate that declines **every** relocation, recording the pids it was asked about — the
/// minimal stand-in for a host's unscanned-root predicate (the same fixture shape
/// `tests/decline_gate.rs` and the `src/ptab.rs` unit tests use).
struct DeclinesAlways {
    asked: Vec<ProcessId>,
}

impl CollectionGate for DeclinesAlways {
    fn declines_relocation(&mut self, receiver: &Process) -> bool {
        self.asked.push(receiver.pid());
        true
    }
}

/// A pid with the given number and serial 0 (only used for the filler's envelope `from`).
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

/// A source arena large enough for the filler and for every expected term built here.
fn source() -> Heap {
    Heap::new(64).expect("the source arena allocates")
}

/// The heap policy of the case fixture: a [`NURSERY_WORDS`]-word nursery, everything else at the
/// BEAM defaults.
fn small_nursery() -> HeapPolicy {
    HeapPolicy {
        initial_young_words: NURSERY_WORDS,
        ..HeapPolicy::default()
    }
}

/// A table plus the atom table its [`SignalAtoms`] were interned from (so tests can compare terms
/// structurally with `compare::eq`) and the atoms themselves.
fn table_with_names() -> (ProcessTable, AtomTable, SignalAtoms) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    (ProcessTable::new(atoms), names, atoms)
}

/// The filler payload: a [`FILLER_WORDS`]-word tuple of distinct small integers.
fn filler_term(src: &mut Heap) -> Eterm {
    let elems: Vec<Eterm> = (0..FILLER_WORDS - 1)
        .map(|i| make_small(i as i64))
        .collect();
    src.tuple(&elems).expect("the filler fits the source arena")
}

/// Spawns the case's receiver — a [`small_nursery`] process — and drains its nursery to exactly
/// [`FREE_WORDS`] free words with a delivered filler message. Returns the receiver and the
/// **delivered** filler payload (its address on the receiver's heap).
fn drained_receiver(table: &mut ProcessTable, filler: Eterm) -> Result<(ProcessId, Eterm)> {
    let receiver = table.spawn(small_nursery())?;
    table.deliver_simple(receiver, pid(99).to_eterm(), filler)?;
    let process = table.get(receiver).expect("the receiver is live");
    assert_eq!(
        process.heap_used_words(),
        FILLER_WORDS,
        "the filler drained the nursery to exactly FREE_WORDS free words"
    );
    assert_eq!(
        process.collector().young().available_words(),
        FREE_WORDS,
        "the fixture's arithmetic holds: one word short of the four-word head build"
    );
    let delivered = process
        .mailbox()
        .peek()
        .expect("the filler is queued")
        .payload();
    Ok((receiver, delivered))
}

/// The receiver's nursery observables: its minor-GC count, its base pointer and its used words.
fn nursery(table: &ProcessTable, pid: ProcessId) -> (u32, *mut Eterm, usize) {
    let process = table.get(pid).expect("the receiver is live");
    (
        process.collector().minor_gcs(),
        process.collector().young().base(),
        process.heap_used_words(),
    )
}

/// The mature-generation words the receiver holds (the promotion/relocation observable).
fn mature_used(table: &ProcessTable, pid: ProcessId) -> usize {
    table
        .get(pid)
        .expect("the receiver is live")
        .collector()
        .mature_used_words()
}

/// Asserts the drained receiver's nursery **cannot** take the four-word head — the precondition
/// that makes the fallback *necessary* rather than incidental. A refused request writes nothing,
/// so the probe leaves the heap exactly as it found it.
fn assert_head_cannot_fit(table: &mut ProcessTable, pid: ProcessId, atoms: &SignalAtoms) {
    let process = table.get_mut(pid).expect("the receiver is live");
    let used = process.heap_used_words();
    let elem = make_small(0);
    assert!(
        process
            .heap_mut()
            .tuple(&[make_atom(atoms.exit()), elem, elem, elem])
            .is_err(),
        "the fixture's nursery really cannot take the four-word head build"
    );
    assert_eq!(
        process.heap_used_words(),
        used,
        "a refused request writes nothing (so the fragment path wrote nothing either)"
    );
}

/// Asserts a declined receiver's heap is **untouched**: no collection, no growth/relocation, not
/// one word written on the nursery, nothing promoted off it.
fn assert_untouched(
    table: &ProcessTable,
    receiver: ProcessId,
    before: (u32, *mut Eterm, usize),
    mature_before: usize,
) {
    let (minor_after, base_after, used_after) = nursery(table, receiver);
    assert_eq!(
        minor_after, before.0,
        "a declined relocation must not collect"
    );
    assert_eq!(
        base_after, before.1,
        "a declined relocation must not relocate or grow the nursery"
    );
    assert_eq!(
        used_after, before.2,
        "the fragment-built signal wrote nothing on the receiver's heap"
    );
    assert_eq!(
        mature_used(table, receiver),
        mature_before,
        "nothing was promoted off the nursery either"
    );
}

/// The single queued message of `pid` — asserted FIFO-behind the fixture's filler.
fn signal_message<'a>(table: &'a ProcessTable, pid: ProcessId) -> &'a Message {
    let mailbox = table.get(pid).expect("the receiver is live").mailbox();
    assert_eq!(
        mailbox.len(),
        2,
        "the filler and the signal, and nothing else: the signal is delivered exactly once"
    );
    mailbox
        .message_at(mailbox.cursor_at(1))
        .expect("the signal is queued behind the filler (FIFO)")
}

/// Asserts `message` is a **fragment-resident** tuple whose `head` (and, for a DOWN, kind) element
/// matches, and returns its payload.
fn assert_fragment_resident(message: &Message, head: Eterm, kind: Option<Eterm>) -> Eterm {
    let fragments = message
        .fragments()
        .expect("the decline's fallback enqueued a fragment-resident message");
    assert_eq!(fragments.len(), 1, "one fragment carries the whole payload");
    let payload = message.payload();
    assert!(
        fragments
            .iter()
            .next()
            .expect("one fragment")
            .contains(payload),
        "the payload root lives inside the fragment"
    );
    // SAFETY: `payload` is the live signal tuple the fragment holds; the element reads below are
    // the head (0) and, for a DOWN, the kind (2) element, in bounds for both shapes.
    let (size, delivered_head) = unsafe { (tuple_size(payload), tuple_element(payload, 0)) };
    assert_eq!(size, if kind.is_some() { 5 } else { 3 });
    assert_eq!(delivered_head, head, "the head atom is the message's own tag");
    if let Some(kind) = kind {
        // SAFETY: a five-tuple, so element 2 is in bounds.
        assert_eq!(unsafe { tuple_element(payload, 2) }, kind);
    }
    payload
}

/// Installs a declining gate — the host valve — asserting the premise the case rests on.
fn install_declining_gate(table: &mut ProcessTable) {
    table.set_collection_gate(Box::new(DeclinesAlways { asked: Vec::new() }));
    assert!(
        table.has_collection_gate(),
        "the host gate is installed for this case"
    );
}

/// Marks the receiver **relocation-exempt** — the crate-owned valve — and asserts the table
/// agrees.
fn exempt(table: &mut ProcessTable, receiver: ProcessId) -> Result<()> {
    table.set_relocation_exempt(receiver, true)?;
    assert!(
        table
            .get(receiver)
            .expect("the receiver is live")
            .is_relocation_exempt(),
        "the receiver is relocation-exempt for this case"
    );
    Ok(())
}

/// Asserts the crate-owned valve is **not** what declined: the receiver is neither exempt nor
/// `Running`, so `has_unpublished_roots` is false for it and only the host gate could have.
fn assert_crate_valve_is_false(table: &ProcessTable, receiver: ProcessId) {
    let process = table.get(receiver).expect("the receiver is live");
    assert!(
        !process.is_relocation_exempt(),
        "the receiver is not relocation-exempt"
    );
    assert_ne!(
        process.state(),
        ProcessState::Running,
        "and not `Running`, so the crate's own gate is false for it"
    );
}

/// Builds an external pid term on `heap` (the frozen 4-word `[header, node, creation, data]`
/// thing).
fn remote_pid(heap: &mut Heap, node: Eterm, number: u32, serial: u32, creation: u32) -> Eterm {
    // SAFETY: `heap` hands out `external_pid_size_words()` (4) exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(external_pid_size_words()).expect("the heap has room") };
    // SAFETY: `p` addresses the four words just handed out and `write_external_pid` writes all of
    // them before the pointer escapes; `node` is a live atom term.
    unsafe { write_external_pid(p, node, number, serial, creation) }
}

/// Asserts the filler is still first in the receiver's mailbox, on the receiver's own heap.
fn assert_filler_first(table: &ProcessTable, receiver: ProcessId, filler: Eterm) {
    let first = table
        .get(receiver)
        .expect("the receiver is live")
        .mailbox()
        .peek()
        .expect("the filler is still first");
    assert_eq!(first.payload(), filler, "the filler kept its place in the queue");
    assert!(
        first.fragments().is_none(),
        "the ordinary delivery has no fragment"
    );
}

// --- builder 1: `deliver_noproc_down` (a born-down monitor on a dead local pid) ----------------

/// A **relocation-exempt** watcher whose nursery cannot take the born-down `{'DOWN', Ref, process,
/// Target, noproc}` still receives it: the build goes into a heap fragment, so nothing on the
/// watcher's heap moves.
#[test]
fn an_exempt_watcher_gets_a_born_down_noproc_down_through_a_fragment() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let (watcher, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, watcher, &atoms);
    // A pid the table has never held: the monitor is born down.
    let target = ProcessId::new(4095, 0);

    exempt(&mut table, watcher)?;
    let before = nursery(&table, watcher);
    let mature_before = mature_used(&table, watcher);
    let reference = table.monitor(watcher, target, MonitorKind::Process)?;
    assert_untouched(&table, watcher, before, mature_before);

    let down = signal_message(&table, watcher);
    let payload = assert_fragment_resident(
        down,
        make_atom(atoms.down()),
        Some(make_atom(atoms.process())),
    );
    assert_eq!(
        down.from(),
        target.to_eterm(),
        "the DOWN's envelope `from` is the dead target pid"
    );
    let reference_term = reference.to_eterm(&mut src)?;
    let expected = src.tuple(&[
        make_atom(atoms.down()),
        reference_term,
        make_atom(atoms.process()),
        target.to_eterm(),
        atoms.noproc_term(),
    ])?;
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered DOWN equals the expected five-tuple element for element"
    );
    assert_filler_first(&table, watcher, filler);

    Ok(())
}

/// The same born-down DOWN with the **host gate** in place of the exemption: the crate-owned valve
/// is false for this watcher, so only the host could have declined the relocation.
#[test]
fn a_declining_gate_routes_a_born_down_noproc_down_through_a_fragment() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let (watcher, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, watcher, &atoms);
    let target = ProcessId::new(4095, 0);

    assert_crate_valve_is_false(&table, watcher);
    install_declining_gate(&mut table);
    let before = nursery(&table, watcher);
    let mature_before = mature_used(&table, watcher);
    let reference = table.monitor(watcher, target, MonitorKind::Process)?;
    assert_untouched(&table, watcher, before, mature_before);

    let down = signal_message(&table, watcher);
    let payload = assert_fragment_resident(
        down,
        make_atom(atoms.down()),
        Some(make_atom(atoms.process())),
    );
    assert_eq!(
        down.from(),
        target.to_eterm(),
        "the DOWN's envelope `from` is the dead target pid"
    );
    let reference_term = reference.to_eterm(&mut src)?;
    let expected = src.tuple(&[
        make_atom(atoms.down()),
        reference_term,
        make_atom(atoms.process()),
        target.to_eterm(),
        atoms.noproc_term(),
    ])?;
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered DOWN equals the expected five-tuple element for element"
    );
    assert_filler_first(&table, watcher, filler);

    Ok(())
}

// --- builder 2: `deliver_named_noproc_down` (a named monitor on an unregistered name) ----------

/// A **relocation-exempt** watcher still receives the born-down named
/// `{'DOWN', Ref, process, {Name, Node}, noproc}` fragment-resident, with the `{Name, Node}` target
/// materialised inside the fragment's build and the envelope `from` the `system` atom.
#[test]
fn an_exempt_watcher_gets_a_named_noproc_down_through_a_fragment() -> Result<()> {
    let (mut table, mut names, atoms) = table_with_names();
    let mut src = source();
    let (watcher, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, watcher, &atoms);
    let name = names.intern("unregistered");
    let node = names.intern("nonode@nohost");

    exempt(&mut table, watcher)?;
    let before = nursery(&table, watcher);
    let mature_before = mature_used(&table, watcher);
    let reference = table.monitor_named(watcher, name, node, MonitorKind::Process)?;
    assert_untouched(&table, watcher, before, mature_before);

    let down = signal_message(&table, watcher);
    let payload = assert_fragment_resident(
        down,
        make_atom(atoms.down()),
        Some(make_atom(atoms.process())),
    );
    assert_eq!(
        down.from(),
        make_atom(atoms.system()),
        "a monitor that failed by name stores the `system` atom as its envelope `from`"
    );
    let reference_term = reference.to_eterm(&mut src)?;
    let target = src.tuple(&[make_atom(name), make_atom(node)])?;
    let expected = src.tuple(&[
        make_atom(atoms.down()),
        reference_term,
        make_atom(atoms.process()),
        target,
        atoms.noproc_term(),
    ])?;
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered named DOWN carries the `{{Name, Node}}` target, not a pid"
    );
    assert_filler_first(&table, watcher, filler);

    Ok(())
}

/// The same named DOWN declined by the **host gate** only.
#[test]
fn a_declining_gate_routes_a_named_noproc_down_through_a_fragment() -> Result<()> {
    let (mut table, mut names, atoms) = table_with_names();
    let mut src = source();
    let (watcher, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, watcher, &atoms);
    let name = names.intern("unregistered");
    let node = names.intern("nonode@nohost");

    assert_crate_valve_is_false(&table, watcher);
    install_declining_gate(&mut table);
    let before = nursery(&table, watcher);
    let mature_before = mature_used(&table, watcher);
    let reference = table.monitor_named(watcher, name, node, MonitorKind::Process)?;
    assert_untouched(&table, watcher, before, mature_before);

    let down = signal_message(&table, watcher);
    let payload = assert_fragment_resident(
        down,
        make_atom(atoms.down()),
        Some(make_atom(atoms.process())),
    );
    assert_eq!(
        down.from(),
        make_atom(atoms.system()),
        "a monitor that failed by name stores the `system` atom as its envelope `from`"
    );
    let reference_term = reference.to_eterm(&mut src)?;
    let target = src.tuple(&[make_atom(name), make_atom(node)])?;
    let expected = src.tuple(&[
        make_atom(atoms.down()),
        reference_term,
        make_atom(atoms.process()),
        target,
        atoms.noproc_term(),
    ])?;
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered named DOWN carries the `{{Name, Node}}` target, not a pid"
    );
    assert_filler_first(&table, watcher, filler);

    Ok(())
}

// --- builder 3: `enqueue_remote_down_message` (a node-down DOWN on a remote monitor) -----------

/// A **relocation-exempt** watcher of a remote pid still receives its
/// `{'DOWN', Ref, process, RemotePid, Reason}` on `node_down`, fragment-resident, with the remote
/// pid materialised into the fragment body and the envelope `from` the remote node atom.
#[test]
fn an_exempt_watcher_gets_a_remote_node_down_through_a_fragment() -> Result<()> {
    let (mut table, mut names, atoms) = table_with_names();
    let mut src = source();
    let (watcher, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, watcher, &atoms);
    let node = make_atom(names.intern("remote@host"));
    let reason = make_small(17);
    let ext = remote_pid(&mut src, node, 7, 8, 9);

    exempt(&mut table, watcher)?;
    let reference = table.monitor_term(watcher, ext, MonitorKind::Process)?;
    let before = nursery(&table, watcher);
    let mature_before = mature_used(&table, watcher);
    table.node_down(node, reason)?;
    assert_untouched(&table, watcher, before, mature_before);

    let down = signal_message(&table, watcher);
    let payload = assert_fragment_resident(
        down,
        make_atom(atoms.down()),
        Some(make_atom(atoms.process())),
    );
    assert_eq!(
        down.from(),
        node,
        "a remote monitor stores the node atom as its envelope `from`"
    );
    let reference_term = reference.to_eterm(&mut src)?;
    let expected = src.tuple(&[
        make_atom(atoms.down()),
        reference_term,
        make_atom(atoms.process()),
        ext,
        reason,
    ])?;
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered DOWN carries the external pid and the node-down reason"
    );
    assert_filler_first(&table, watcher, filler);

    Ok(())
}

/// The same remote node-down DOWN declined by the **host gate** only.
#[test]
fn a_declining_gate_routes_a_remote_node_down_through_a_fragment() -> Result<()> {
    let (mut table, mut names, atoms) = table_with_names();
    let mut src = source();
    let (watcher, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, watcher, &atoms);
    let node = make_atom(names.intern("remote@host"));
    let reason = make_small(17);
    let ext = remote_pid(&mut src, node, 7, 8, 9);

    assert_crate_valve_is_false(&table, watcher);
    install_declining_gate(&mut table);
    let reference = table.monitor_term(watcher, ext, MonitorKind::Process)?;
    let before = nursery(&table, watcher);
    let mature_before = mature_used(&table, watcher);
    table.node_down(node, reason)?;
    assert_untouched(&table, watcher, before, mature_before);

    let down = signal_message(&table, watcher);
    let payload = assert_fragment_resident(
        down,
        make_atom(atoms.down()),
        Some(make_atom(atoms.process())),
    );
    assert_eq!(
        down.from(),
        node,
        "a remote monitor stores the node atom as its envelope `from`"
    );
    let reference_term = reference.to_eterm(&mut src)?;
    let expected = src.tuple(&[
        make_atom(atoms.down()),
        reference_term,
        make_atom(atoms.process()),
        ext,
        reason,
    ])?;
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered DOWN carries the external pid and the node-down reason"
    );
    assert_filler_first(&table, watcher, filler);

    Ok(())
}

// --- builder 4: `enqueue_remote_exit_message` (a node-down EXIT on a trapping remote link) -----

/// A **relocation-exempt**, **trapping** peer linked to a remote pid still traps the node-down
/// exit: `{'EXIT', RemotePid, Reason}` is delivered fragment-resident, with the external pid in the
/// body and the envelope `from` the remote node atom.
#[test]
fn an_exempt_trapping_peer_gets_a_remote_node_exit_through_a_fragment() -> Result<()> {
    let (mut table, mut names, atoms) = table_with_names();
    let mut src = source();
    let (peer, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, peer, &atoms);
    let node = make_atom(names.intern("remote@host"));
    let reason = make_small(17);
    let ext = remote_pid(&mut src, node, 7, 8, 9);

    table.get_mut(peer).expect("the peer is live").set_trap_exit(true);
    exempt(&mut table, peer)?;
    table.link_term(peer, ext)?;
    let before = nursery(&table, peer);
    let mature_before = mature_used(&table, peer);
    table.node_down(node, reason)?;
    assert!(table.contains(peer), "the trapping peer survives the node down");
    assert_untouched(&table, peer, before, mature_before);

    let exit = signal_message(&table, peer);
    let payload = assert_fragment_resident(exit, make_atom(atoms.exit()), None);
    assert_eq!(
        exit.from(),
        node,
        "a remote link stores the node atom as its envelope `from`"
    );
    let expected = src.tuple(&[make_atom(atoms.exit()), ext, reason])?;
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered EXIT carries the external pid and the node-down reason"
    );
    assert_filler_first(&table, peer, filler);

    Ok(())
}

/// The same remote node-down EXIT declined by the **host gate** only.
#[test]
fn a_declining_gate_routes_a_remote_node_exit_through_a_fragment() -> Result<()> {
    let (mut table, mut names, atoms) = table_with_names();
    let mut src = source();
    let (peer, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, peer, &atoms);
    let node = make_atom(names.intern("remote@host"));
    let reason = make_small(17);
    let ext = remote_pid(&mut src, node, 7, 8, 9);

    table.get_mut(peer).expect("the peer is live").set_trap_exit(true);
    assert_crate_valve_is_false(&table, peer);
    install_declining_gate(&mut table);
    table.link_term(peer, ext)?;
    let before = nursery(&table, peer);
    let mature_before = mature_used(&table, peer);
    table.node_down(node, reason)?;
    assert!(table.contains(peer), "the trapping peer survives the node down");
    assert_untouched(&table, peer, before, mature_before);

    let exit = signal_message(&table, peer);
    let payload = assert_fragment_resident(exit, make_atom(atoms.exit()), None);
    assert_eq!(
        exit.from(),
        node,
        "a remote link stores the node atom as its envelope `from`"
    );
    let expected = src.tuple(&[make_atom(atoms.exit()), ext, reason])?;
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered EXIT carries the external pid and the node-down reason"
    );
    assert_filler_first(&table, peer, filler);

    Ok(())
}

// --- builder 5: `enqueue_port_down_message` (a port-close DOWN on a port monitor) --------------

/// A **relocation-exempt** watcher of a port still receives its
/// `{'DOWN', Ref, port, Port, Reason}` on `port_closed`, fragment-resident, with the envelope
/// `from` the port itself.
#[test]
fn an_exempt_watcher_gets_a_port_down_through_a_fragment() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let (watcher, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, watcher, &atoms);
    let owner = table.spawn_default()?;
    let mut ports = PortTable::new();
    let port = ports.alloc_owned(owner)?;
    let reference = RefAllocator::new().next();
    ports.monitor(port, watcher, reference)?;
    let reason = make_small(17);

    exempt(&mut table, watcher)?;
    let before = nursery(&table, watcher);
    let mature_before = mature_used(&table, watcher);
    table.port_closed(&mut ports, port, reason)?;
    assert_untouched(&table, watcher, before, mature_before);

    let down = signal_message(&table, watcher);
    let payload =
        assert_fragment_resident(down, make_atom(atoms.down()), Some(make_atom(atoms.port())));
    assert_eq!(
        down.from(),
        port.to_eterm(),
        "a port signal stores the port itself as its envelope `from`"
    );
    let reference_term = reference.to_eterm(&mut src)?;
    let expected = src.tuple(&[
        make_atom(atoms.down()),
        reference_term,
        make_atom(atoms.port()),
        port.to_eterm(),
        reason,
    ])?;
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered port DOWN carries the port and the close reason"
    );
    assert_filler_first(&table, watcher, filler);

    Ok(())
}

/// The same port DOWN declined by the **host gate** only.
#[test]
fn a_declining_gate_routes_a_port_down_through_a_fragment() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let (watcher, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, watcher, &atoms);
    let owner = table.spawn_default()?;
    let mut ports = PortTable::new();
    let port = ports.alloc_owned(owner)?;
    let reference = RefAllocator::new().next();
    ports.monitor(port, watcher, reference)?;
    let reason = make_small(17);

    assert_crate_valve_is_false(&table, watcher);
    install_declining_gate(&mut table);
    let before = nursery(&table, watcher);
    let mature_before = mature_used(&table, watcher);
    table.port_closed(&mut ports, port, reason)?;
    assert_untouched(&table, watcher, before, mature_before);

    let down = signal_message(&table, watcher);
    let payload =
        assert_fragment_resident(down, make_atom(atoms.down()), Some(make_atom(atoms.port())));
    assert_eq!(
        down.from(),
        port.to_eterm(),
        "a port signal stores the port itself as its envelope `from`"
    );
    let reference_term = reference.to_eterm(&mut src)?;
    let expected = src.tuple(&[
        make_atom(atoms.down()),
        reference_term,
        make_atom(atoms.port()),
        port.to_eterm(),
        reason,
    ])?;
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered port DOWN carries the port and the close reason"
    );
    assert_filler_first(&table, watcher, filler);

    Ok(())
}

// --- builder 6: `enqueue_port_exit_message` (a port-close EXIT on a trapping port link) --------

/// A **relocation-exempt**, **trapping** peer linked to a port still traps the close:
/// `{'EXIT', Port, Reason}` is delivered fragment-resident, with the envelope `from` the port.
#[test]
fn an_exempt_trapping_peer_gets_a_port_exit_through_a_fragment() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let (peer, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, peer, &atoms);
    let owner = table.spawn_default()?;
    let mut ports = PortTable::new();
    let port = ports.alloc_owned(owner)?;
    ports.link(port, peer)?;
    let reason = make_small(17);

    table.get_mut(peer).expect("the peer is live").set_trap_exit(true);
    exempt(&mut table, peer)?;
    let before = nursery(&table, peer);
    let mature_before = mature_used(&table, peer);
    table.port_closed(&mut ports, port, reason)?;
    assert!(table.contains(peer), "the trapping peer survives the close");
    assert_untouched(&table, peer, before, mature_before);

    let exit = signal_message(&table, peer);
    let payload = assert_fragment_resident(exit, make_atom(atoms.exit()), None);
    assert_eq!(
        exit.from(),
        port.to_eterm(),
        "a port signal stores the port itself as its envelope `from`"
    );
    let expected = src.tuple(&[make_atom(atoms.exit()), port.to_eterm(), reason])?;
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered port EXIT carries the port and the close reason"
    );
    assert_filler_first(&table, peer, filler);

    Ok(())
}

/// The same port EXIT declined by the **host gate** only.
#[test]
fn a_declining_gate_routes_a_port_exit_through_a_fragment() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let (peer, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, peer, &atoms);
    let owner = table.spawn_default()?;
    let mut ports = PortTable::new();
    let port = ports.alloc_owned(owner)?;
    ports.link(port, peer)?;
    let reason = make_small(17);

    table.get_mut(peer).expect("the peer is live").set_trap_exit(true);
    assert_crate_valve_is_false(&table, peer);
    install_declining_gate(&mut table);
    let before = nursery(&table, peer);
    let mature_before = mature_used(&table, peer);
    table.port_closed(&mut ports, port, reason)?;
    assert!(table.contains(peer), "the trapping peer survives the close");
    assert_untouched(&table, peer, before, mature_before);

    let exit = signal_message(&table, peer);
    let payload = assert_fragment_resident(exit, make_atom(atoms.exit()), None);
    assert_eq!(
        exit.from(),
        port.to_eterm(),
        "a port signal stores the port itself as its envelope `from`"
    );
    let expected = src.tuple(&[make_atom(atoms.exit()), port.to_eterm(), reason])?;
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered port EXIT carries the port and the close reason"
    );
    assert_filler_first(&table, peer, filler);

    Ok(())
}

// --- the control: with no decline in force every builder collects and grows -------------------

/// Runs `trigger` on a saturated receiver with **no** decline in force and asserts the ordinary
/// arm: exactly one collection, an on-demand nursery growth/relocation, and an **on-heap** message
/// (no fragment) whose head/kind match the expected signal shape.
///
/// This is the controlled-experiment counterpart of every `*_declined_*` case above — it pins that
/// the fragment route is an *additional* decline, not a replacement for the grow-and-retry path
/// each builder always had.
fn assert_would_collect<F>(
    table: &mut ProcessTable,
    receiver: ProcessId,
    head: Eterm,
    kind: Option<Eterm>,
    trigger: F,
) -> Result<()>
where
    F: FnOnce(&mut ProcessTable) -> Result<()>,
{
    let minor_before = table
        .get(receiver)
        .expect("the receiver is live")
        .collector()
        .minor_gcs();
    trigger(table)?;

    let message = signal_message(table, receiver);
    assert!(
        message.fragments().is_none(),
        "with no decline in force the build goes to the receiver's own heap, never a fragment"
    );
    let payload = message.payload();
    // SAFETY: `payload` is the live signal tuple the builder produced; the head (0) and, for a
    // DOWN, the kind (2) element are in bounds for both shapes.
    let (size, delivered_head) = unsafe { (tuple_size(payload), tuple_element(payload, 0)) };
    assert_eq!(size, if kind.is_some() { 5 } else { 3 });
    assert_eq!(delivered_head, head, "the head atom is the message's own tag");
    if let Some(kind) = kind {
        // SAFETY: a five-tuple, so element 2 is in bounds.
        assert_eq!(unsafe { tuple_element(payload, 2) }, kind);
    }

    let process = table.get(receiver).expect("the receiver is live");
    assert!(
        process.collector().minor_gcs() > minor_before,
        "the refused build collected (the relocation was not declined)"
    );
    assert!(
        process.collector().young().contains_term(payload),
        "the retried build landed on the receiver's own (new) nursery"
    );
    Ok(())
}

/// The **contrast** for all six builders: with no gate and no exemption the identical trigger
/// takes the ordinary arm — one collection, a grown/relocated nursery and an on-heap message — so
/// the fragment route the cases above pin is a genuine *decline*, not the crate's default.
#[test]
fn without_a_decline_each_builder_collects_and_grows() -> Result<()> {
    // Builder 1: `deliver_noproc_down`.
    {
        let (mut table, _names, atoms) = table_with_names();
        let mut src = source();
        let (watcher, _filler) = drained_receiver(&mut table, filler_term(&mut src))?;
        assert_head_cannot_fit(&mut table, watcher, &atoms);
        let target = ProcessId::new(4095, 0);
        assert_would_collect(
            &mut table,
            watcher,
            make_atom(atoms.down()),
            Some(make_atom(atoms.process())),
            |t| t.monitor(watcher, target, MonitorKind::Process).map(|_| ()),
        )?;
    }

    // Builder 2: `deliver_named_noproc_down`.
    {
        let (mut table, mut names, atoms) = table_with_names();
        let mut src = source();
        let (watcher, _filler) = drained_receiver(&mut table, filler_term(&mut src))?;
        assert_head_cannot_fit(&mut table, watcher, &atoms);
        let name = names.intern("unregistered");
        let node = names.intern("nonode@nohost");
        assert_would_collect(
            &mut table,
            watcher,
            make_atom(atoms.down()),
            Some(make_atom(atoms.process())),
            |t| t.monitor_named(watcher, name, node, MonitorKind::Process).map(|_| ()),
        )?;
    }

    // Builder 3: `enqueue_remote_down_message`.
    {
        let (mut table, mut names, atoms) = table_with_names();
        let mut src = source();
        let (watcher, _filler) = drained_receiver(&mut table, filler_term(&mut src))?;
        assert_head_cannot_fit(&mut table, watcher, &atoms);
        let node = make_atom(names.intern("remote@host"));
        let ext = remote_pid(&mut src, node, 7, 8, 9);
        table.monitor_term(watcher, ext, MonitorKind::Process)?;
        assert_would_collect(
            &mut table,
            watcher,
            make_atom(atoms.down()),
            Some(make_atom(atoms.process())),
            |t| t.node_down(node, make_small(17)),
        )?;
    }

    // Builder 4: `enqueue_remote_exit_message`.
    {
        let (mut table, mut names, atoms) = table_with_names();
        let mut src = source();
        let (peer, _filler) = drained_receiver(&mut table, filler_term(&mut src))?;
        assert_head_cannot_fit(&mut table, peer, &atoms);
        let node = make_atom(names.intern("remote@host"));
        let ext = remote_pid(&mut src, node, 7, 8, 9);
        table.get_mut(peer).expect("the peer is live").set_trap_exit(true);
        table.link_term(peer, ext)?;
        assert_would_collect(&mut table, peer, make_atom(atoms.exit()), None, |t| {
            t.node_down(node, make_small(17))
        })?;
    }

    // Builder 5: `enqueue_port_down_message`.
    {
        let (mut table, _names, atoms) = table_with_names();
        let mut src = source();
        let (watcher, _filler) = drained_receiver(&mut table, filler_term(&mut src))?;
        assert_head_cannot_fit(&mut table, watcher, &atoms);
        let owner = table.spawn_default()?;
        let mut ports = PortTable::new();
        let port = ports.alloc_owned(owner)?;
        ports.monitor(port, watcher, RefAllocator::new().next())?;
        assert_would_collect(
            &mut table,
            watcher,
            make_atom(atoms.down()),
            Some(make_atom(atoms.port())),
            |t| t.port_closed(&mut ports, port, make_small(17)),
        )?;
    }

    // Builder 6: `enqueue_port_exit_message`.
    {
        let (mut table, _names, atoms) = table_with_names();
        let mut src = source();
        let (peer, _filler) = drained_receiver(&mut table, filler_term(&mut src))?;
        assert_head_cannot_fit(&mut table, peer, &atoms);
        let owner = table.spawn_default()?;
        let mut ports = PortTable::new();
        let port = ports.alloc_owned(owner)?;
        ports.link(port, peer)?;
        table.get_mut(peer).expect("the peer is live").set_trap_exit(true);
        assert_would_collect(&mut table, peer, make_atom(atoms.exit()), None, |t| {
            t.port_closed(&mut ports, port, make_small(17))
        })?;
    }

    Ok(())
}
