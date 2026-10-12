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

//! Integration evidence for the **host collection gate** (`CollectionGate`) of
//! [`ProcessTable`](beam_process::ProcessTable).
//!
//! The crate's own "must not move this heap" gate is
//! [`Process::has_unpublished_roots`](beam_process::Process): true for a **relocation-exempt**
//! process and for a currently **`Running`** one (`tests/signal_fragment_fallback.rs` and
//! `tests/gc_exemption.rs` pin both). That gate is *narrower* than the real hazard: a receiver the
//! crate cannot classify — e.g. one just woken `Waiting → Runnable` by a delivery, whose
//! `Process::regs` snapshot is **not** its whole live root set — passes it, and a signal-driven
//! collection may then relocate its heap under live, unpublished terms.
//!
//! `CollectionGate` is the host's half of the same policy: a predicate the host installs
//! ([`ProcessTable::set_collection_gate`](beam_process::ProcessTable::set_collection_gate)) and the
//! table consults at every signal site plus
//! [`ProcessTable::deliver`](beam_process::ProcessTable::deliver), so a host that knows a
//! receiver's live root set is not fully scanned by its collector can refuse the *relocation* —
//! while the notification is still delivered, through the heap-fragment route the
//! `Running`/relocation-exempt receiver already uses.
//!
//! The cases:
//!
//! 1. [`a_host_gate_declines_an_oversized_link_exit_into_a_heap_fragment`] — a **trapping**,
//!    **non-exempt**, **non-`Running`** receiver linked to a dying peer with a reason larger than
//!    its whole nursery: the enqueue-time reason copy is refused, the signal never reaches the
//!    queue, and the `{'EXIT', …}` is delivered fragment-resident, with the nursery neither
//!    collected nor moved and the blocked receiver woken.
//! 2. [`a_host_gate_declines_an_oversized_monitor_down_into_a_heap_fragment`] — the same for a
//!    **watcher**'s `{'DOWN', …}`.
//! 3. [`a_host_gate_declines_the_delivery_collect_and_the_flag_is_restored`] — the
//!    **message-delivery** half: `ProcessTable::deliver` surfaces the copy engine's **raw**
//!    `AllocKind::Heap`/`AllocKind::Message` refusal (the caller's heap-fragment key) with the
//!    nursery untouched, and the temporary exemption the gate implies is restored afterwards.
//! 4. [`without_the_gate_the_same_signals_collect_and_grow`] — the **control**: with no gate
//!    installed the identical refusal takes the ordinary arm — one collection, an on-demand nursery
//!    growth and a relocation — so the gate is an *additional* decline, not a replacement.
//!
//! Determinism: no sleep, no wall-clock bound and no oracle. Every size is derived from the public
//! heap-policy arithmetic, and every "did nothing" claim is checked against the receiver's own
//! nursery observables (`minor_gcs`, the young space's base pointer and used words, the mature
//! generation's used words), so the refusal is exact and forced by capacity, never by timing.

use beam_alloc::Heap;
use beam_common::{AllocKind, BeamError, Result};
use beam_gc::{next_heap_size, HeapPolicy};
use beam_message::{Message, OffHeapTable};
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::list::ptr_val;
use beam_term::prelude::{
    make_atom, make_small, size_object, tuple_element, tuple_size, AtomTable, Eterm,
};

/// The words a `{'EXIT', From, Reason}` tuple occupies on the receiver's heap: `1` header plus
/// the three elements. This is also what the `{'DOWN', …}` build allocates **first** — its boxed
/// monitor reference is four words (`1` header + `3` payload).
const HEAD_WORDS: usize = 4;

/// The free words the receiver's nursery is drained to: the head build's first allocation minus
/// one, so the build is refused **by capacity** at its very first allocation and therefore writes
/// nothing.
const FREE_WORDS: usize = HEAD_WORDS - 1;

/// The receiver's nursery in words. Deliberately smaller than the policy's 64-word reserve
/// ([`HeapPolicy::need_reserve_words`]), which is what makes the *control* case's collection
/// observable: the emptied nursery is replaced by the smallest curated size covering the reserve,
/// so it grows and the base pointer moves.
const NURSERY_WORDS: usize = HEAD_WORDS + FREE_WORDS;

/// The filler message's size in words: `1` header plus two immediate elements — exactly the words
/// the drain occupies, leaving [`FREE_WORDS`] free.
const FILLER_WORDS: usize = NURSERY_WORDS - FREE_WORDS;

/// The words the enqueue-time cases' reason occupies: one word more than the receiver's **whole**
/// nursery, so the *reason copy* onto the receiver's own heap is the refused request.
const OVERSIZED_REASON_WORDS: usize = NURSERY_WORDS + 1;

/// A host gate that declines **every** relocation, recording the pids it was asked about — the
/// minimal stand-in for a host's unscanned-root predicate.
struct DeclinesAlways {
    asked: Vec<ProcessId>,
}

impl CollectionGate for DeclinesAlways {
    fn declines_relocation(&mut self, receiver: &Process) -> bool {
        self.asked.push(receiver.pid());
        true
    }
}

/// A pid with the given number and serial 0 (only used for an envelope `from` here).
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

/// A source arena large enough for the filler and for every term built here.
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

/// A table plus the atom table its [`SignalAtoms`] were interned from (so tests can compare
/// terms structurally with `compare::eq`) and the atoms themselves.
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

/// The enqueue-time cases' reason: an [`OVERSIZED_REASON_WORDS`]-word tuple of distinct small
/// integers, so it cannot fit the receiver's nursery and a truncated or duplicated copy cannot
/// pass a structural comparison.
fn oversized_reason(src: &mut Heap) -> Eterm {
    let elems: Vec<Eterm> = (0..OVERSIZED_REASON_WORDS - 1)
        .map(|i| make_small(i as i64))
        .collect();
    src.tuple(&elems)
        .expect("the oversized reason fits the source arena")
}

/// Spawns the case's receiver — a [`small_nursery`] process — and drains its nursery to exactly
/// [`FREE_WORDS`] free words with a delivered filler message. Returns the receiver and the
/// **delivered** filler payload (its address on the receiver's heap is a relocation probe).
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
        "the fixture's arithmetic holds: one word short of the head build"
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

/// Asserts the drained receiver's nursery **cannot** take the four-word head the fragment path
/// bypasses — the precondition that makes the fallback *necessary* rather than incidental.
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

/// Asserts `message` is a fragment-resident [`HEAD_WORDS`]-word-tier tuple whose head (and, for a
/// DOWN, kind) element matches, and returns its payload.
fn assert_fragment_resident(message: &Message, head: Eterm, kind: Option<Eterm>) -> Eterm {
    let fragments = message
        .fragments()
        .expect("the gate's fallback enqueued a fragment-resident message");
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

/// Installs a declining gate and returns nothing beyond the table's own accessor confirmation —
/// the one line every case repeats, kept here so the "the crate's gate is *not* what declined"
/// premise is asserted at the point of installation.
fn install_declining_gate(table: &mut ProcessTable) {
    table.set_collection_gate(Box::new(DeclinesAlways { asked: Vec::new() }));
    assert!(
        table.has_collection_gate(),
        "the host gate is installed for this case"
    );
}

/// A **trapping**, non-exempt, non-`Running` receiver linked to a dying peer with a reason larger
/// than its whole nursery still traps the exit: the **enqueue-time** reason copy is refused by the
/// host gate, the signal never reaches the queue, and the `{'EXIT', …}` is delivered
/// fragment-resident — with no collection, no growth, no relocation, not one word written on the
/// receiver's heap, and a blocked receiver woken by the delivery.
#[test]
fn a_host_gate_declines_an_oversized_link_exit_into_a_heap_fragment() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let (receiver, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    let reason = oversized_reason(&mut src);
    assert!(
        size_object(reason) > FREE_WORDS,
        "the reason is larger than the receiver's free space"
    );
    let dying = table.spawn_default()?;

    // A trapping receiver survives the exit and receives it as a message; the link makes the
    // dying partner's death the signal's source.
    table
        .get_mut(receiver)
        .expect("the receiver is live")
        .set_trap_exit(true);
    table.link(dying, receiver)?;
    let expected = src.tuple(&[make_atom(atoms.exit()), dying.to_eterm(), reason])?;

    // The premise of the case: the crate's OWN gate is false for this receiver — it is neither
    // relocation-exempt nor `Running` — so only the host gate can decline the move.
    {
        let process = table.get(receiver).expect("the receiver is live");
        assert!(
            !process.is_relocation_exempt(),
            "the receiver is not relocation-exempt"
        );
        assert_ne!(
            process.state(),
            ProcessState::Running,
            "and not `Running`, so `has_unpublished_roots` is false for it"
        );
    }
    install_declining_gate(&mut table);
    // A **blocked** receiver: the queued path used to wake it when the signal was applied, so the
    // enqueue-time route has to wake it itself.
    table
        .get_mut(receiver)
        .expect("the receiver is live")
        .set_state(ProcessState::Waiting);
    let (minor_before, base_before, used_before) = nursery(&table, receiver);
    let mature_before = mature_used(&table, receiver);

    table.terminate(dying, reason)?;
    assert!(table.contains(receiver), "the trapping receiver survived");

    // The declined receiver's heap is **untouched**.
    let (minor_after, base_after, used_after) = nursery(&table, receiver);
    assert_eq!(
        minor_after, minor_before,
        "a declined relocation must not collect"
    );
    assert_eq!(
        base_after, base_before,
        "a declined relocation must not relocate or grow the nursery"
    );
    assert_eq!(
        used_after, used_before,
        "the fragment-built EXIT wrote nothing on the receiver's heap"
    );
    assert_eq!(
        mature_used(&table, receiver),
        mature_before,
        "nothing was promoted off the nursery either"
    );
    // The signal never reached the queue: its reason could not live on the receiver's heap.
    assert_eq!(
        table.pending_signals(receiver)?,
        0,
        "the refused reason copy left no queued signal behind"
    );
    // ... but the blocked receiver was woken all the same.
    assert_eq!(
        table.drain_woken(),
        vec![receiver],
        "the delivery woke the blocked receiver"
    );

    // ... and the EXIT is nevertheless delivered, whole, FIFO behind the filler.
    let exit = signal_message(&table, receiver);
    let payload = assert_fragment_resident(exit, make_atom(atoms.exit()), None);
    // SAFETY: `payload` is the live three-tuple the fragment holds; elements 1 and 2 are in
    // bounds.
    let (from, delivered) = unsafe { (tuple_element(payload, 1), tuple_element(payload, 2)) };
    assert_eq!(from, dying.to_eterm(), "the signaller is the dying link partner");
    assert!(
        compare::eq(delivered, reason, &names),
        "the oversized reason is delivered whole (the copy went into the fragment)"
    );
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered EXIT equals the expected three-tuple element for element"
    );

    // FIFO: the fixture's filler is still first and still on the receiver's heap.
    let mailbox = table.get(receiver).expect("the receiver is live").mailbox();
    let first = mailbox.peek().expect("the filler is still first");
    assert_eq!(first.payload(), filler, "the filler kept its place in the queue");
    assert!(first.fragments().is_none(), "the ordinary delivery has no fragment");

    Ok(())
}

/// The same decline for a **monitor-DOWN**: a non-exempt, non-`Running` **watcher** whose monitor
/// reason is larger than its whole nursery still receives its `{'DOWN', Ref, process, Target,
/// Reason}` — the refused enqueue-time copy never queues a signal, and the DOWN is delivered
/// fragment-resident with the nursery untouched.
#[test]
fn a_host_gate_declines_an_oversized_monitor_down_into_a_heap_fragment() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let (watcher, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    let reason = oversized_reason(&mut src);
    assert!(
        size_object(reason) > FREE_WORDS,
        "the reason is larger than the watcher's free space"
    );
    let target = table.spawn_default()?;

    // The expected term: the same five-tuple the table must build, on the source arena.
    let reference = table.monitor(watcher, target, MonitorKind::Process)?;
    let reference_term = reference.to_eterm(&mut src)?;
    let expected = src.tuple(&[
        make_atom(atoms.down()),
        reference_term,
        make_atom(atoms.process()),
        target.to_eterm(),
        reason,
    ])?;

    {
        let process = table.get(watcher).expect("the watcher is live");
        assert!(
            !process.is_relocation_exempt(),
            "the watcher is not relocation-exempt"
        );
        assert_ne!(
            process.state(),
            ProcessState::Running,
            "and not `Running`, so `has_unpublished_roots` is false for it"
        );
    }
    install_declining_gate(&mut table);
    table
        .get_mut(watcher)
        .expect("the watcher is live")
        .set_state(ProcessState::Waiting);
    let (minor_before, base_before, used_before) = nursery(&table, watcher);
    let mature_before = mature_used(&table, watcher);

    table.terminate(target, reason)?;
    assert!(table.contains(watcher), "the watcher survived the target");

    let (minor_after, base_after, used_after) = nursery(&table, watcher);
    assert_eq!(
        minor_after, minor_before,
        "a declined relocation must not collect"
    );
    assert_eq!(
        base_after, base_before,
        "a declined relocation must not relocate or grow the nursery"
    );
    assert_eq!(
        used_after, used_before,
        "the fragment-built DOWN wrote nothing on the watcher's heap"
    );
    assert_eq!(
        mature_used(&table, watcher),
        mature_before,
        "nothing was promoted off the nursery either"
    );
    assert_eq!(
        table.pending_signals(watcher)?,
        0,
        "the refused reason copy left no queued signal behind"
    );
    assert_eq!(
        table.drain_woken(),
        vec![watcher],
        "the delivery woke the blocked watcher"
    );

    let down = signal_message(&table, watcher);
    let payload =
        assert_fragment_resident(down, make_atom(atoms.down()), Some(make_atom(atoms.process())));
    // SAFETY: `payload` is the live five-tuple the fragment holds; elements 1, 3 and 4 are in
    // bounds.
    let (delivered_ref, delivered_target, delivered_reason) = unsafe {
        (
            tuple_element(payload, 1),
            tuple_element(payload, 3),
            tuple_element(payload, 4),
        )
    };
    assert!(
        compare::eq(delivered_ref, reference_term, &names),
        "the DOWN carries the monitor's own reference (a reference is a boxed term, so its two \
         materialisations are equal by content, not by address)"
    );
    assert_eq!(delivered_target, target.to_eterm(), "the DOWN names the target pid");
    assert!(
        compare::eq(delivered_reason, reason, &names),
        "the oversized reason is delivered whole (the copy went into the fragment)"
    );
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered DOWN equals the expected five-tuple element for element"
    );

    let mailbox = table.get(watcher).expect("the watcher is live").mailbox();
    let first = mailbox.peek().expect("the filler is still first");
    assert_eq!(first.payload(), filler, "the filler kept its place in the queue");
    assert!(first.fragments().is_none(), "the ordinary delivery has no fragment");

    Ok(())
}

/// The **message-delivery** half: when the gate declines,
/// [`ProcessTable::deliver`](beam_process::ProcessTable::deliver) marks the receiver
/// relocation-exempt for exactly one
/// [`Process::deliver`](beam_process::Process) call, so the copy engine's **raw**
/// `AllocKind::Heap`/`AllocKind::Message` refusal — the exact value the host's heap-fragment
/// fallback keys on — reaches the caller with the nursery untouched, and the receiver's own
/// exemption flag is restored (an ordinary receiver is left exactly as it was).
#[test]
fn a_host_gate_declines_the_delivery_collect_and_the_flag_is_restored() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let (receiver, _filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, receiver, &atoms);
    // One word more than the three free words the fixture left.
    let payload = src.tuple(&[make_small(100), make_small(101), make_small(102)])?;
    assert_eq!(
        size_object(payload),
        HEAD_WORDS,
        "the fixture's payload is exactly the refused tier"
    );
    let from = pid(99).to_eterm();

    install_declining_gate(&mut table);
    let (minor_before, base_before, used_before) = nursery(&table, receiver);
    let mature_before = mature_used(&table, receiver);

    // The delivery surfaces the RAW refusal and enqueues nothing.
    match table.deliver_simple(receiver, from, payload) {
        Err(BeamError::Alloc(failure)) => assert!(
            matches!(failure.kind, AllocKind::Heap | AllocKind::Message),
            "the raw refusal keeps its Heap/Message kind, got {:?}",
            failure.kind
        ),
        other => panic!("expected the raw allocation refusal, got {other:?}"),
    }

    // ... and the heap is untouched, with the gate's temporary exemption already restored.
    assert_eq!(
        nursery(&table, receiver),
        (minor_before, base_before, used_before),
        "a declined delivery must not collect, grow, relocate or write"
    );
    assert_eq!(
        mature_used(&table, receiver),
        mature_before,
        "nothing was promoted off the nursery either"
    );
    assert_eq!(
        table.get(receiver).expect("the receiver is live").mailbox().len(),
        1,
        "the refused delivery enqueued nothing (only the fixture's filler is queued)"
    );
    assert!(
        !table
            .get(receiver)
            .expect("the receiver is live")
            .is_relocation_exempt(),
        "the temporary exemption the gate implies is restored after the one delivery"
    );

    // The control inside the same fixture: the gate taken away, the identical delivery takes the
    // ordinary collect-and-grow arm and lands on the receiver's own (new) nursery.
    assert!(table.take_collection_gate().is_some());
    assert!(!table.has_collection_gate());
    let src_off_heap = OffHeapTable::new();
    table.deliver(receiver, from, payload, &src_off_heap)?;
    let delivered = {
        let process = table.get_mut(receiver).expect("the receiver is live");
        process.receive(|_| true).expect("the filler");
        let message = process.receive(|_| true).expect("the payload behind the filler");
        assert!(
            message.fragments().is_none(),
            "the non-declined arm builds on the receiver's own heap, never in a fragment"
        );
        message.payload()
    };
    let process = table.get(receiver).expect("the receiver is live");
    assert!(
        process.collector().minor_gcs() > minor_before,
        "the non-declined arm collected (the relocation was not declined)"
    );
    assert!(
        process.collector().young().contains_term(delivered),
        "the retried build landed on the receiver's own (new) nursery"
    );
    assert!(
        compare::eq(delivered, payload, &names),
        "the delivered payload equals the source term element for element"
    );
    assert!(
        !process.collector().mature_contains(ptr_val(delivered)),
        "the new message is not a promoted (mature) object"
    );

    Ok(())
}

/// The **contrast**: with no gate installed, the identical oversized signals take the other arm of
/// the policy — one collection, an on-demand growth of the nursery and a relocation, with the
/// message built **on the receiver's new heap** and no fragment at all.
///
/// This is what pins that the gate is an *additional* decline rather than a replacement for the
/// grow-and-retry path the crate always had.
#[test]
fn without_the_gate_the_same_signals_collect_and_grow() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let (receiver, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, receiver, &atoms);
    let dying = table.spawn_default()?;
    table
        .get_mut(receiver)
        .expect("the receiver is live")
        .set_trap_exit(true);
    table.link(dying, receiver)?;
    // The refusal this case exercises is the **apply-time** build, so the reason is the immediate
    // `normal` atom (which the enqueue-time copy takes without allocating).
    let reason = make_atom(atoms.normal());
    let expected = src.tuple(&[make_atom(atoms.exit()), dying.to_eterm(), reason])?;

    assert!(
        !table.has_collection_gate(),
        "no gate is installed: this is the crate's default arm"
    );
    assert!(!table
        .get(receiver)
        .expect("the receiver is live")
        .is_relocation_exempt());
    let (minor_before, base_before, used_before) = nursery(&table, receiver);
    assert_eq!(used_before, FILLER_WORDS, "the filler occupies the nursery");
    let capacity_before = table
        .get(receiver)
        .expect("the receiver is live")
        .collector()
        .young()
        .capacity_words();
    let reserve = table
        .get(receiver)
        .expect("the receiver is live")
        .collector()
        .policy()
        .need_reserve_words;

    table.terminate(dying, reason)?;
    assert!(table.contains(receiver), "the trapping receiver survived");

    // Exactly one collection ran ...
    let (minor_after, base_after, used_after) = nursery(&table, receiver);
    assert_eq!(
        minor_after,
        minor_before + 1,
        "the refused build collected exactly once before retrying"
    );
    // ... the emptied, mostly-empty nursery was replaced: it grew and it moved ...
    let capacity_after = table
        .get(receiver)
        .expect("the receiver is live")
        .collector()
        .young()
        .capacity_words();
    assert!(
        capacity_after > capacity_before,
        "the nursery was grown on demand: {capacity_before} -> {capacity_after} words"
    );
    assert_eq!(
        capacity_after,
        next_heap_size(reserve, 0),
        "the growth targets the smallest curated size covering the policy's reserve"
    );
    assert_ne!(
        base_after, base_before,
        "the replacement nursery is a relocation (the base pointer moves)"
    );
    // ... the queued filler was relocated into the mature generation ...
    let filler_after = table
        .get(receiver)
        .expect("the receiver is live")
        .mailbox()
        .peek()
        .expect("the filler is still first")
        .payload();
    assert_ne!(filler_after, filler, "the collection relocated the queued filler");
    assert!(
        table
            .get(receiver)
            .expect("the receiver is live")
            .collector()
            .mature_contains(ptr_val(filler_after)),
        "the promoted filler now lives in the mature generation"
    );
    // ... and the exit message landed **on the receiver's heap**, with no fragment involved.
    assert_eq!(
        used_after,
        HEAD_WORDS,
        "the EXIT tuple is the receiver's new nursery's only occupant"
    );
    let exit = signal_message(&table, receiver);
    assert!(
        exit.fragments().is_none(),
        "without a gate the build goes to the receiver's own heap, never a fragment"
    );
    let payload = exit.payload();
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered EXIT equals the expected three-tuple element for element"
    );
    assert!(
        table
            .get(receiver)
            .expect("the receiver is live")
            .collector()
            .young()
            .contains_term(payload),
        "the retried build landed on the receiver's own (new) nursery"
    );

    Ok(())
}
