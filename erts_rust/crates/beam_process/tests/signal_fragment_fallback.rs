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

//! Integration evidence for the **heap-fragment fallback** of
//! [`ProcessTable`](beam_process::ProcessTable)'s signal-delivery path.
//!
//! A signal whose message cannot be built on the receiving process's own young heap is a
//! *normal* condition, and the two arms of the exhaustion policy handle it differently
//! (`src/ptab.rs`'s `build_signal_message` for an apply-time build, `deliver_head_signal` for
//! a queued signal):
//!
//! * an **ordinary** receiver has its nursery collected and grown **on demand**
//!   ([`Process::ensure_capacity`](beam_process::Process) — the shared
//!   `grow_receiver_for_signal`) and the build is retried once, so the message ends up on the
//!   receiver's own heap;
//! * a **relocation-exempt** receiver
//!   ([`Process::set_relocation_exempt`](beam_process::Process)) must **not** have its heap move
//!   (the host holds live words outside the crate's root set, so relocating would recycle them),
//!   so the payload is built on a 64-word scratch arena and copied **whole** into a fresh
//!   `beam_message::HeapFragment` the enqueued `Message` owns (`enqueue_fragment_signal`): no
//!   collection, no growth, and **nothing written on the receiver's heap** at all.
//!
//! The fragment model is the C's: `erl_proc_sig_queue.c` builds every signal message into the
//! message's **own** heap fragment, so a signal that cannot live on the receiver's heap is not
//! lost — the exempt arm reproduces that model exactly where the M1 heap layout allows it.
//!
//! The three cases are one **controlled experiment**: the same receiver state, the same
//! immediate reason, the same failing build — the *only* difference is the exemption flag.
//!
//! 1. [`an_exempt_receiver_gets_a_monitor_down_through_a_heap_fragment`] — a saturated-nursery
//!    relocation-exempt **watcher** still receives its `{'DOWN', Ref, process, Target, noproc}`,
//!    fragment-resident and whole, with the nursery untouched.
//! 2. [`an_exempt_receiver_gets_a_link_exit_through_a_heap_fragment`] — the same for a trapping
//!    receiver's `{'EXIT', Dying, normal}`.
//! 3. [`a_non_exempt_receiver_collects_relocates_and_grows_instead_of_using_a_fragment`] — the
//!    same refusal on a receiver that is **not** exempt takes the *other* arm: one collection,
//!    an on-demand nursery growth and a relocation, and the message lands on the receiver's
//!    (new) heap with no fragment — the fallback did not replace the grow-and-retry path.
//!
//! Cases 1 and 2 are the regression: on the pre-fix code each of them fails with the raw
//! `BeamError::Alloc(AllocFailure { requested: 32, kind: Heap })` returned out of `terminate`,
//! with **no** message queued (the signal was lost) — verified by running this very file against
//! the pre-fix commit `360a4c6536`, where cases 1 and 2 fail with that error and case 3 passes.
//! Case 3 pins that the non-exempt arm is *unchanged* — it is the arm the pre-fix code already
//! had, and it is the reason the two exempt cases are a controlled experiment rather than a
//! broad-brush "signals work" check.
//!
//! Determinism: no sleep, no wall-clock bound and no oracle. Every size is derived from the
//! public heap-policy arithmetic — [`HeapPolicy::initial_young_words`] (the nursery), the
//! four-word `{'EXIT', …}` head, [`HeapPolicy::need_reserve_words`] and [`next_heap_size`] (the
//! growth the collection's resize computes) — so the refusal is exact and the relocation is
//! forced by capacity, never by timing.

use beam_alloc::Heap;
use beam_common::Result;
use beam_gc::{next_heap_size, HeapPolicy};
use beam_message::Message;
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::list::ptr_val;
use beam_term::prelude::{make_atom, make_small, tuple_element, tuple_size, AtomTable, Eterm};

/// The words a `{'EXIT', From, Reason}` tuple occupies on the receiver's heap: `1` header plus
/// the three elements. This is also what the `{'DOWN', …}` build allocates **first** — its
/// boxed monitor reference is four words (`1` header + `3` payload) — so one number covers
/// "what the head build needs before it can fail".
const HEAD_WORDS: usize = 4;

/// The free words the receiver's nursery is drained to: the head build's first allocation minus
/// one, so the build is refused **by capacity** at its very first allocation and therefore
/// writes nothing (a request that does not fit is rejected before any word is handed out).
const FREE_WORDS: usize = HEAD_WORDS - 1;

/// The receiver's nursery in words. Deliberately smaller than the policy's 64-word reserve
/// ([`HeapPolicy::need_reserve_words`]), which is what makes case 3's relocation observable: the
/// collection empties the nursery, and the resize that follows replaces a nursery the round left
/// mostly empty with the smallest curated size covering the reserve — so the capacity grows and
/// the base pointer moves. (A nursery the reserve already fits is simply kept, which is why the
/// default 233-word nursery could not have shown the growth.)
const NURSERY_WORDS: usize = HEAD_WORDS + FREE_WORDS;

/// The filler message's size in words: `1` header plus two immediate elements — exactly the
/// words the drain occupies, leaving [`FREE_WORDS`] free.
const FILLER_WORDS: usize = NURSERY_WORDS - FREE_WORDS;

/// A pid with the given number and serial 0 (only used for an envelope `from` here).
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

/// A table plus the atom table its [`SignalAtoms`] were interned from (so tests can compare
/// terms structurally with `compare::eq`) and the atoms themselves.
fn table_with_names() -> (ProcessTable, AtomTable, SignalAtoms) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    (ProcessTable::new(atoms), names, atoms)
}

/// The filler payload: a [`FILLER_WORDS`]-word tuple of distinct small integers, so a truncated
/// or duplicated copy cannot pass a structural comparison.
fn filler_term(src: &mut Heap) -> Eterm {
    let elems: Vec<Eterm> = (0..FILLER_WORDS - 1)
        .map(|i| make_small(i as i64))
        .collect();
    src.tuple(&elems).expect("the filler fits the source arena")
}

/// Spawns the case's receiver — a [`small_nursery`] process — and drains its nursery to exactly
/// [`FREE_WORDS`] free words with a delivered (and therefore queue-resident) filler message.
/// Returns the receiver and the **delivered** filler payload: its address on the receiver's heap
/// is the relocation probe case 3 uses.
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
///
/// The probe writes a four-word boxed term through the receiver's own (public) heap writer — the
/// same footprint as the `{'EXIT', …}` tuple and as the `{'DOWN', …}` build's **first**
/// allocation, its boxed four-word monitor reference — and asserts that the request is refused
/// and that a refused request writes **nothing**: the capacity preflight is what the "not one
/// word written on the receiver's heap" claim rests on.
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

/// The single queued message of `pid` — asserted FIFO-behind the fixture's filler — with the
/// evidence that it is **not** on the receiver's heap.
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

/// Asserts `message` is a fragment-resident [`HEAD_WORDS`]-word-tier tuple whose `head`/`kind`
/// elements and whole structure match, and returns its payload.
fn assert_fragment_resident(message: &Message, head: Eterm, kind: Option<Eterm>) -> Eterm {
    let fragments = message
        .fragments()
        .expect("the exempt fallback enqueued a fragment-resident message");
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

/// A **relocation-exempt**, saturated-nursery **watcher** still receives its
/// `{'DOWN', Ref, process, Target, noproc}`: the build cannot land on its heap (which must not
/// move under the host's unpublished roots), so the message is built on a scratch arena and
/// copied whole into a heap fragment the queued `Message` owns — no collection, no growth, no
/// relocation, and not one word written on the receiver's heap.
///
/// The reason is the immediate `noproc` atom, so the enqueue-time reason copy allocates nothing
/// and the *apply-time* message build is the only refused request this case exercises.
#[test]
fn an_exempt_receiver_gets_a_monitor_down_through_a_heap_fragment() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let reason = make_atom(atoms.noproc());
    let (watcher, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, watcher, &atoms);
    let target = table.spawn_default()?;

    // The expected term: the same five-tuple the table must build, on the source arena. The
    // reference is materialised through the same public writer the table uses.
    let reference = table.monitor(watcher, target, MonitorKind::Process)?;
    let reference_term = reference.to_eterm(&mut src)?;
    let expected = src.tuple(&[
        make_atom(atoms.down()),
        reference_term,
        make_atom(atoms.process()),
        target.to_eterm(),
        reason,
    ])?;

    // The exemption is opt-in per process, and it gates the *fallback*, not the fixture.
    assert!(!table
        .get(watcher)
        .expect("the watcher is live")
        .is_relocation_exempt());
    table.set_relocation_exempt(watcher, true)?;
    let (minor_before, base_before, used_before) = nursery(&table, watcher);
    let mature_before = mature_used(&table, watcher);

    // The target dies with the immediate reason; an ordinary watcher would collect and grow.
    table.terminate(target, reason)?;
    assert!(table.contains(watcher), "the watcher survived the target");

    // The whole point: the exempt receiver's heap is **untouched**.
    let (minor_after, base_after, used_after) = nursery(&table, watcher);
    assert_eq!(
        minor_after, minor_before,
        "a relocation-exempt receiver must not collect"
    );
    assert_eq!(
        base_after, base_before,
        "a relocation-exempt receiver's nursery must not be relocated or grown"
    );
    assert_eq!(
        used_after, used_before,
        "the fragment-built DOWN wrote nothing on the receiver's heap"
    );
    assert_eq!(
        mature_used(&table, watcher),
        mature_before,
        "nothing was promoted off the nursery either"
    );

    // ... and the DOWN is nevertheless delivered, whole, FIFO behind the filler.
    let down = signal_message(&table, watcher);
    let payload = assert_fragment_resident(down, make_atom(atoms.down()), Some(make_atom(atoms.process())));
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
        "the DOWN carries the monitor's own reference (compared structurally: a reference is a \
         boxed term, so its two materialisations are equal by content, not by address)"
    );
    assert_eq!(delivered_target, target.to_eterm(), "the DOWN names the target pid");
    assert_eq!(delivered_reason, reason, "the reason is delivered verbatim");
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered DOWN equals the expected five-tuple element for element"
    );

    // FIFO: the fixture's filler is still first and still on the receiver's heap.
    let mailbox = table.get(watcher).expect("the watcher is live").mailbox();
    let first = mailbox.peek().expect("the filler is still first");
    assert_eq!(first.payload(), filler, "the filler kept its place in the queue");
    assert!(
        first.fragments().is_none(),
        "the ordinary delivery still lives on the receiver's heap"
    );

    Ok(())
}

/// The same fallback for a **link exit**: a trapping, relocation-exempt receiver whose nursery
/// cannot take the `{'EXIT', From, Reason}` tuple still traps the signal — fragment-resident,
/// with its nursery and its queue positions untouched.
#[test]
fn an_exempt_receiver_gets_a_link_exit_through_a_heap_fragment() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let reason = make_atom(atoms.normal());
    let (receiver, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, receiver, &atoms);
    let dying = table.spawn_default()?;

    // A trapping receiver survives the exit and queues it as a message; the link makes the
    // dying partner's death the signal's source.
    table.get_mut(receiver).expect("the receiver is live").set_trap_exit(true);
    table.link(dying, receiver)?;
    let expected = src.tuple(&[make_atom(atoms.exit()), dying.to_eterm(), reason])?;

    table.set_relocation_exempt(receiver, true)?;
    let (minor_before, base_before, used_before) = nursery(&table, receiver);
    let mature_before = mature_used(&table, receiver);

    table.terminate(dying, reason)?;
    assert!(table.contains(receiver), "the trapping receiver survived");

    let (minor_after, base_after, used_after) = nursery(&table, receiver);
    assert_eq!(minor_after, minor_before, "a relocation-exempt receiver must not collect");
    assert_eq!(
        base_after, base_before,
        "a relocation-exempt receiver's nursery must not be relocated or grown"
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

    let exit = signal_message(&table, receiver);
    let payload = assert_fragment_resident(exit, make_atom(atoms.exit()), None);
    // SAFETY: `payload` is the live three-tuple the fragment holds; elements 1 and 2 are in
    // bounds.
    let (from, delivered) = unsafe { (tuple_element(payload, 1), tuple_element(payload, 2)) };
    assert_eq!(from, dying.to_eterm(), "the signaller is the dying link partner");
    assert_eq!(delivered, reason, "the reason is delivered verbatim");
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered EXIT equals the expected three-tuple element for element"
    );

    let mailbox = table.get(receiver).expect("the receiver is live").mailbox();
    let first = mailbox.peek().expect("the filler is still first");
    assert_eq!(first.payload(), filler, "the filler kept its place in the queue");
    assert!(first.fragments().is_none(), "the ordinary delivery has no fragment");

    Ok(())
}

/// The **contrast**: the identical refusal on a receiver that is *not* relocation-exempt takes
/// the other arm of the policy — one collection, an on-demand growth of the nursery, and the
/// retry lands the message **on the receiver's new heap** with no fragment at all.
///
/// This is what pins that the fragment fallback did not replace the grow-and-retry path: the
/// collection promotes the queued filler into the mature generation (a relocation), the resize
/// replaces the emptied nursery with the smallest curated size covering the policy's reserve
/// (a growth, so the base pointer moves), and the `{'EXIT', …}` is built on that fresh nursery.
#[test]
fn a_non_exempt_receiver_collects_relocates_and_grows_instead_of_using_a_fragment() -> Result<()> {
    let (mut table, names, atoms) = table_with_names();
    let mut src = source();
    let reason = make_atom(atoms.normal());
    let (receiver, filler) = drained_receiver(&mut table, filler_term(&mut src))?;
    assert_head_cannot_fit(&mut table, receiver, &atoms);
    let dying = table.spawn_default()?;
    table.get_mut(receiver).expect("the receiver is live").set_trap_exit(true);
    table.link(dying, receiver)?;
    let expected = src.tuple(&[make_atom(atoms.exit()), dying.to_eterm(), reason])?;

    // Not exempt: the receiver takes the collect-and-grow arm (this is the crate default).
    assert!(!table
        .get(receiver)
        .expect("the receiver is live")
        .is_relocation_exempt());
    let (minor_before, base_before, used_before) = nursery(&table, receiver);
    assert_eq!(
        used_before, FILLER_WORDS,
        "the fixture's filler occupies the nursery (three free words) before the signal"
    );
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
    // ... and the exit message landed **on the receiver's heap**: the whole build was retried
    // there, with no fragment involved.
    assert_eq!(
        used_after,
        HEAD_WORDS,
        "the EXIT tuple is the receiver's new nursery's only occupant"
    );

    let exit = signal_message(&table, receiver);
    assert!(
        exit.fragments().is_none(),
        "the non-exempt arm builds on the receiver's own heap, never in a fragment"
    );
    let payload = exit.payload();
    // SAFETY: `payload` is the live three-tuple the receiver's nursery holds; elements 0..=2 are
    // in bounds.
    let (size, head, from, delivered) = unsafe {
        (
            tuple_size(payload),
            tuple_element(payload, 0),
            tuple_element(payload, 1),
            tuple_element(payload, 2),
        )
    };
    assert_eq!(size, 3, "the exit message is the three-tuple EXIT/From/Reason");
    assert_eq!(head, make_atom(atoms.exit()));
    assert_eq!(from, dying.to_eterm(), "the signaller is the dying link partner");
    assert_eq!(delivered, reason, "the reason is delivered verbatim");
    assert!(
        compare::eq(payload, expected, &names),
        "the delivered EXIT equals the expected three-tuple element for element"
    );
    // The message really is **on the receiver's young heap** (not merely fragment-free): its
    // root addresses the nursery the collection just installed, and it is not a promoted object.
    assert!(
        table
            .get(receiver)
            .expect("the receiver is live")
            .collector()
            .young()
            .contains_term(payload),
        "the retried build landed on the receiver's own (new) nursery"
    );
    assert!(
        !table
            .get(receiver)
            .expect("the receiver is live")
            .collector()
            .mature_contains(ptr_val(payload)),
        "the new message is not a promoted (mature) object"
    );

    Ok(())
}
