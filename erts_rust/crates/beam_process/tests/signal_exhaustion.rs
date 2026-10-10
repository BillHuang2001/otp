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

//! Integration evidence for the **signal-delivery exhaustion policy** of
//! [`ProcessTable`](beam_process::ProcessTable).
//!
//! The signal builders share [`Process::deliver`](beam_process::Process)'s exhaustion policy: a
//! signal copied onto a full receiver heap is a *normal* condition, so the nursery is collected
//! and grown **on demand** to cover the refused request and the build is retried once — a
//! `{'EXIT', …}`/`{'DOWN', …}` reason larger than the bounded nursery is still delivered, whole
//! and exactly once. A **relocation-exempt** receiver
//! ([`Process::set_relocation_exempt`](beam_process::Process)) instead surfaces the **raw**
//! `BeamError::Alloc(AllocKind::Heap | AllocKind::Message)` refusal unchanged — no collection,
//! no growth, no retry — because the host holds live words outside the crate root set and
//! relocating would recycle them.
//!
//! The suite pins both halves of the policy from the observable end state:
//!
//! 1. [`an_oversized_exit_reason_grows_the_receivers_nursery_and_is_delivered`] — a link exit
//!    whose copied reason is one word larger than the receiver's whole nursery is delivered: the
//!    nursery grew, one collection ran, and `{'EXIT', From, Reason}` lands whole and once, in
//!    FIFO order ahead of a following delivery.
//! 2. [`an_oversized_down_reason_grows_the_watchers_nursery_and_is_delivered`] — the same for a
//!    monitor `{'DOWN', …}` carrying a large reason.
//! 3. [`a_relocation_exempt_receiver_surfaces_the_raw_refusal_without_relocating`] — an exempt
//!    receiver surfaces the raw `Alloc` refusal and neither the minor-GC counter nor the nursery
//!    base pointer moves.
//! 4. [`a_refused_signal_leaves_no_partial_state_and_the_receiver_still_delivers`] — the typed
//!    failure leaves no partial enqueue (the signal is not queued, the mailbox is empty) and the
//!    receiver is not wedged: a fitting signal is delivered normally afterwards.
//!
//! Determinism: no sleep, no wall-clock bound and no oracle. Every size is derived from the
//! public heap-policy arithmetic ([`next_heap_size`], the bounded nursery of a fresh `Process`),
//! and exhaustion is forced by a payload one word larger than that nursery, so the refusal is
//! exact. The source payloads are built on a plain [`Heap`] arena, so the 233-word default
//! nursery of a `Process` never limits the payload size.

use beam_alloc::Heap;
use beam_common::{AllocKind, BeamError, Result};
use beam_gc::next_heap_size;
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{
    is_immediate, make_small, size_object, tuple_element, tuple_size, AtomTable, Eterm,
};

/// A pid with the given number and serial 0 (only used for `from`/message terms here).
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

/// A source arena: every oversized reason payload and its expected copy is built here,
/// comfortably larger than the 233-word nursery of a fresh `Process`.
fn source() -> Heap {
    Heap::new(4096).expect("the source heap allocates")
}

/// The elements of a tuple occupying exactly `words` heap words (`1` header + `words - 1`
/// elements, per `size_object`). The elements are distinct small integers, so a shifted,
/// duplicated or truncated copy cannot pass the structural comparison.
fn tuple_elems(words: usize) -> Vec<Eterm> {
    (0..words.saturating_sub(1))
        .map(|i| make_small(i as i64))
        .collect()
}

/// A table plus the atom table its [`SignalAtoms`] were interned from, so tests can compare
/// terms structurally (`compare::eq`) against the same atom index space.
fn table_with_names() -> (ProcessTable, AtomTable) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    (ProcessTable::new(atoms), names)
}

/// The payload of `pid`'s single queued message (panics unless exactly one is queued).
fn sole_payload(table: &ProcessTable, pid: ProcessId) -> Eterm {
    let mailbox = table.get(pid).expect("the recipient is live").mailbox();
    assert_eq!(mailbox.len(), 1, "exactly one message is queued");
    mailbox.peek().expect("one message is queued").payload()
}

/// A link exit whose copied reason is one word larger than the receiver's whole bounded nursery
/// is delivered: the refused copy collected once and grew the nursery **on demand**, and
/// `{'EXIT', From, Reason}` lands in the mailbox whole and exactly once, ahead of a following
/// delivery (FIFO preserved).
#[test]
fn an_oversized_exit_reason_grows_the_receivers_nursery_and_is_delivered() -> Result<()> {
    let (mut table, names) = table_with_names();
    let a = table.spawn_default()?;
    let b = table.spawn_default()?;
    // A trapping receiver survives the exit and queues it as a message.
    table.get_mut(b).expect("b is live").set_trap_exit(true);
    table.link(a, b)?;

    let mut src = source();
    // One word larger than b's whole bounded nursery: the first copy is refused, and a bounded
    // nursery cannot fit it by collecting alone -- the retry must grow it on demand.
    let capacity = table
        .get(b)
        .expect("b is live")
        .collector()
        .young()
        .capacity_words();
    let payload_words = capacity + 1;
    let elems = tuple_elems(payload_words);
    let reason = src
        .tuple(&elems)
        .expect("the oversized reason fits the source heap");
    assert_eq!(size_object(reason), payload_words, "header + elements");

    let minor_before = table.get(b).expect("b is live").collector().minor_gcs();
    let base_before = table.get(b).expect("b is live").collector().young().base();

    // `a` dies with the oversized reason; the trapping `b` must receive it whole.
    table.terminate(a, reason)?;
    assert!(table.contains(b), "the trapping receiver survived");

    // The refused copy collected exactly once ...
    assert_eq!(
        table
            .get(b)
            .expect("b survived")
            .collector()
            .minor_gcs(),
        minor_before + 1,
        "the refused copy collected exactly once before retrying"
    );
    // ... and grew the nursery on demand to the smallest curated size covering the request,
    // which replaces (relocates) it.
    assert_eq!(
        table
            .get(b)
            .expect("b survived")
            .collector()
            .young()
            .capacity_words(),
        next_heap_size(payload_words, 0),
        "the retry grew the nursery on demand to the smallest size covering the request"
    );
    assert_ne!(
        table
            .get(b)
            .expect("b survived")
            .collector()
            .young()
            .base(),
        base_before,
        "the on-demand growth replaces the nursery (a relocation)"
    );

    // Exactly one `{'EXIT', a, reason}` message, whole.
    let payload = sole_payload(&table, b);
    // SAFETY: `payload` is the live `{'EXIT', A, Reason}` tuple the table built on B's heap; the
    // three element slots are in bounds.
    let (size, from, delivered) =
        unsafe { (tuple_size(payload), tuple_element(payload, 1), tuple_element(payload, 2)) };
    assert_eq!(size, 3, "the exit message is the three-tuple EXIT/From/Reason");
    assert_eq!(from, a.to_eterm(), "the signaller is the dying link partner");
    assert!(!is_immediate(delivered), "the structured reason was copied");
    assert_eq!(
        size_object(delivered),
        payload_words,
        "the reason was delivered whole -- no truncation"
    );
    let expected = src
        .tuple(&elems)
        .expect("the expected reason fits the source heap");
    assert!(
        compare::eq(delivered, expected, &names),
        "the delivered reason equals the sent one element for element"
    );

    // FIFO preserved: a following delivery queues behind the exit message, which is received
    // first.
    let other = pid(99).to_eterm();
    table.deliver_simple(b, other, make_small(123))?;
    assert_eq!(table.get(b).expect("b is live").mailbox().len(), 2);
    let first = table
        .get_mut(b)
        .expect("b is live")
        .receive(|_| true)
        .expect("the exit message")
        .payload();
    assert_eq!(first, payload, "the exit message is still first in FIFO order");
    let second = table
        .get_mut(b)
        .expect("b is live")
        .receive(|_| true)
        .expect("the following message")
        .payload();
    assert_eq!(second, make_small(123));

    Ok(())
}

/// A monitor `{'DOWN', …}` carrying a reason larger than the watcher's whole nursery is
/// delivered the same way: the refused copy collected once and grew the nursery on demand, and
/// the DOWN lands whole.
#[test]
fn an_oversized_down_reason_grows_the_watchers_nursery_and_is_delivered() -> Result<()> {
    let (mut table, names) = table_with_names();
    let watcher = table.spawn_default()?;
    let target = table.spawn_default()?;
    table.monitor(watcher, target, MonitorKind::Process)?;

    let mut src = source();
    let capacity = table
        .get(watcher)
        .expect("the watcher is live")
        .collector()
        .young()
        .capacity_words();
    let payload_words = capacity + 1;
    let elems = tuple_elems(payload_words);
    let reason = src
        .tuple(&elems)
        .expect("the oversized reason fits the source heap");

    let minor_before = table
        .get(watcher)
        .expect("the watcher is live")
        .collector()
        .minor_gcs();
    let base_before = table
        .get(watcher)
        .expect("the watcher is live")
        .collector()
        .young()
        .base();

    // The target dies with the oversized reason; the watcher's DOWN must carry it whole.
    table.terminate(target, reason)?;
    assert!(table.contains(watcher), "the watcher survived the target");

    assert_eq!(
        table
            .get(watcher)
            .expect("the watcher survived")
            .collector()
            .minor_gcs(),
        minor_before + 1,
        "the refused copy collected exactly once before retrying"
    );
    assert_eq!(
        table
            .get(watcher)
            .expect("the watcher survived")
            .collector()
            .young()
            .capacity_words(),
        next_heap_size(payload_words, 0),
        "the retry grew the nursery on demand to the smallest size covering the request"
    );
    assert_ne!(
        table
            .get(watcher)
            .expect("the watcher survived")
            .collector()
            .young()
            .base(),
        base_before,
        "the on-demand growth replaces the nursery (a relocation)"
    );

    let payload = sole_payload(&table, watcher);
    // SAFETY: `payload` is the live `{'DOWN', Ref, process, Target, Reason}` tuple the table
    // built on the watcher's heap; the five element slots are in bounds.
    let (size, delivered) = unsafe { (tuple_size(payload), tuple_element(payload, 4)) };
    assert_eq!(size, 5, "the DOWN is the five-tuple DOWN/Ref/Kind/Target/Reason");
    assert!(!is_immediate(delivered), "the structured reason was copied");
    assert_eq!(
        size_object(delivered),
        payload_words,
        "the reason was delivered whole -- no truncation"
    );
    let expected = src
        .tuple(&elems)
        .expect("the expected reason fits the source heap");
    assert!(
        compare::eq(delivered, expected, &names),
        "the delivered reason equals the sent one element for element"
    );

    Ok(())
}

/// A **relocation-exempt** receiver surfaces the **raw** allocation refusal unchanged and the
/// heap is left completely untouched: no collection ran and the nursery base did not move. Both
/// assertions fail the instant the signal path collects/grows despite the exemption.
#[test]
fn a_relocation_exempt_receiver_surfaces_the_raw_refusal_without_relocating() -> Result<()> {
    let (mut table, _names) = table_with_names();
    let signaller = table.spawn_default()?;
    let receiver = table.spawn_default()?;

    // Not exempt by default -- the exemption is opt-in per process.
    assert!(!table
        .get(receiver)
        .expect("the receiver is live")
        .is_relocation_exempt());
    table.set_relocation_exempt(receiver, true)?;

    let mut src = source();
    // One word larger than the whole nursery: refused by the copier's preflight (which writes
    // nothing) -- the exempt path must surface this refusal verbatim rather than collect/grow.
    let capacity = table
        .get(receiver)
        .expect("the receiver is live")
        .collector()
        .young()
        .capacity_words();
    let payload_words = capacity + 1;
    let elems = tuple_elems(payload_words);
    let reason = src
        .tuple(&elems)
        .expect("the oversized reason fits the source heap");

    let minor_before = table
        .get(receiver)
        .expect("the receiver is live")
        .collector()
        .minor_gcs();
    let base_before = table
        .get(receiver)
        .expect("the receiver is live")
        .collector()
        .young()
        .base();
    let used_before = table.get(receiver).expect("the receiver is live").heap_used_words();
    assert_eq!(used_before, 0, "a fresh receiver's nursery is empty");

    match table.send_exit(signaller, receiver, reason) {
        Err(BeamError::Alloc(failure)) => assert!(
            matches!(failure.kind, AllocKind::Heap | AllocKind::Message),
            "the raw refusal keeps its Heap/Message kind, got {:?}",
            failure.kind
        ),
        other => panic!("expected the raw allocation refusal, got {other:?}"),
    }

    let r = table.get(receiver).expect("the receiver survived");
    assert_eq!(
        r.collector().minor_gcs(),
        minor_before,
        "a relocation-exempt signal must not collect"
    );
    assert_eq!(
        r.collector().young().base(),
        base_before,
        "a relocation-exempt signal must not relocate (or grow) the nursery"
    );
    assert_eq!(
        r.heap_used_words(),
        used_before,
        "a refused signal writes nothing (capacity is preflighted)"
    );
    assert_eq!(r.mailbox().len(), 0, "a refused signal is not enqueued");

    Ok(())
}

/// The typed failure leaves **no partial state**: the signal was not queued at all and the
/// mailbox is still empty, and the receiver is not wedged -- a fitting signal is trapped and
/// delivered exactly once afterwards.
#[test]
fn a_refused_signal_leaves_no_partial_state_and_the_receiver_still_delivers() -> Result<()> {
    let (mut table, _names) = table_with_names();
    let signaller = table.spawn_default()?;
    let receiver = table.spawn_default()?;
    table.get_mut(receiver).expect("the receiver is live").set_trap_exit(true);
    table.set_relocation_exempt(receiver, true)?;

    let mut src = source();
    let capacity = table
        .get(receiver)
        .expect("the receiver is live")
        .collector()
        .young()
        .capacity_words();
    let payload_words = capacity + 1;
    let elems = tuple_elems(payload_words);
    let reason = src
        .tuple(&elems)
        .expect("the oversized reason fits the source heap");

    // The oversized trapped exit is refused with the raw allocation error ...
    assert!(matches!(
        table.send_exit(signaller, receiver, reason),
        Err(BeamError::Alloc(_))
    ));
    // ... leaving no partial enqueue: the signal is not queued, the mailbox is empty.
    assert_eq!(
        table.pending_signals(receiver)?,
        0,
        "the refused signal was not queued at all"
    );
    assert_eq!(
        table.get(receiver).expect("the receiver is live").mailbox().len(),
        0
    );

    // The receiver is not wedged: a fitting signal is trapped and delivered exactly once.
    table.send_exit(signaller, receiver, make_small(5))?;
    let payload = sole_payload(&table, receiver);
    // SAFETY: `payload` is the live `{'EXIT', Signaller, 5}` tuple the table built on the
    // receiver's heap; the three element slots are in bounds.
    let (size, from, delivered) =
        unsafe { (tuple_size(payload), tuple_element(payload, 1), tuple_element(payload, 2)) };
    assert_eq!(size, 3);
    assert_eq!(from, signaller.to_eterm());
    assert_eq!(delivered, make_small(5));
    assert_eq!(
        table.pending_signals(receiver)?,
        0,
        "the delivered signal left the queue"
    );

    Ok(())
}
