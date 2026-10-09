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

//! Integration evidence for [`Process::deliver`]'s **collect-and-retry** on heap exhaustion.
//!
//! `Process::deliver` treats `BeamError::Alloc(AllocKind::Heap | AllocKind::Message)` as a
//! *normal* condition: it collects the process's young heap once and retries the copy, and any
//! other (or second) failure is propagated untouched. That branch had no coverage — this file
//! exercises it deterministically, from both sides:
//!
//! 1. [`deliver_retries_once_after_a_collection_and_enqueues_exactly_one_message`] — the copy is
//!    refused on a full nursery, the retry's collection grows the nursery one curated step, and
//!    the payload is enqueued **whole** and **once**.
//! 2. [`the_collection_leaves_the_heap_usable_for_a_second_delivery`] — the retried copy left the
//!    nursery partly used, so the next, larger payload takes the same path again: the collection
//!    left a usable heap, not a wedged one.
//! 3. [`a_payload_that_cannot_fit_is_a_typed_error_and_leaves_the_mailbox_untouched`] — a
//!    genuinely unfittable payload (a nursery bounded by the heap policy) is the typed
//!    `BeamError::Alloc`, with the mailbox and the heap left exactly as they were. The copier
//!    preflights capacity before writing a word, so there is no partial write and no half message.
//!
//! Determinism: every number below is derived from the public heap-policy arithmetic
//! ([`beam_gc::next_heap_size`], `HeapPolicy::from_min_max`), never from a wall-clock bound or a
//! sleep; the retry is forced by *filling the nursery exactly*, so no timing is involved. Source
//! terms are built on a plain [`Heap`] arena (the pattern `tests/split_borrow.rs` uses), so the
//! 233-word default nursery of a `Process` never limits the payload size.

use beam_alloc::Heap;
use beam_common::{AllocKind, BeamError, Result};
use beam_gc::{next_heap_size, HeapPolicy};
use beam_message::OffHeapTable;
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{make_small, size_object, AtomTable, Eterm};

/// A pid with the given number and serial 0 (the process table is not involved in these tests).
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

/// A source arena: every payload tuple and every expected tuple in this suite is built here,
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

/// Fills `process`'s nursery **completely** with unrooted garbage, so the next copy of anything at
/// all is refused and the collection that follows sees a full round.
///
/// Returns the nursery's capacity, which is also the number of words now used.
fn fill_nursery(process: &mut Process) -> usize {
    let capacity = process.collector().young().capacity_words();
    // A tuple of `capacity - 1` elements occupies `capacity` words: the arityval header plus the
    // elements. Unrooted, so the collection that follows reclaims all of it.
    let filler = vec![make_small(0); capacity - 1];
    process
        .heap_mut()
        .tuple(&filler)
        .expect("the nursery fills exactly");
    assert_eq!(process.heap_used_words(), capacity);
    assert_eq!(process.collector().young().available_words(), 0);
    capacity
}

/// A payload larger than the *free* space is refused, the single retry collects (which grows the
/// nursery one curated step), and the payload then lands in the mailbox whole and exactly once.
#[test]
fn deliver_retries_once_after_a_collection_and_enqueues_exactly_one_message() -> Result<()> {
    let mut receiver = Process::new(pid(1))?;
    let mut src = source();

    let capacity = fill_nursery(&mut receiver);
    // The size a collection resizes a *filled* nursery to: one curated step past its capacity
    // (`HeapPolicy::young_after_collection` -> `next_words(capacity, 1)`).
    let grown = next_heap_size(capacity, 1);
    assert!(grown > capacity);

    // Larger than the free space (0 words), small enough for the grown nursery: the first copy
    // must fail, the retry must succeed.
    let payload_words = grown / 2;
    assert!(payload_words > 0 && payload_words <= grown);
    let elems = tuple_elems(payload_words);
    let payload = src
        .tuple(&elems)
        .expect("the payload tuple fits the source heap");
    assert_eq!(size_object(payload), payload_words, "header + elements");

    let from = pid(2).to_eterm();
    let outcome = receiver.deliver(from, payload, &OffHeapTable::new())?;
    assert!(matches!(
        outcome,
        DeliverOutcome::Enqueued | DeliverOutcome::Woke
    ));

    // The collection really is what made room: the retry's collect ran exactly once and resized
    // the nursery.
    assert_eq!(
        receiver.collector().minor_gcs(),
        1,
        "the refused copy collected exactly once before retrying"
    );
    assert_eq!(
        receiver.collector().young().capacity_words(),
        grown,
        "the refused copy collected, and that collection grew the nursery one step"
    );

    // Exactly ONE message: the retry re-enqueues, it does not enqueue twice.
    assert_eq!(
        receiver.mailbox().len(),
        1,
        "the retry enqueues exactly one message, never a duplicate"
    );
    let message = receiver.mailbox().peek().expect("one message is queued");
    assert_eq!(message.from(), from);

    // Whole, not truncated: the copied payload is the same term graph.
    let copied = message.payload();
    assert_eq!(
        size_object(copied),
        payload_words,
        "the payload was copied whole -- no truncation"
    );
    let expected = src
        .tuple(&elems)
        .expect("the expected tuple fits the source heap");
    assert!(
        compare::eq(copied, expected, &AtomTable::new()),
        "the copied payload equals the sent one element for element"
    );

    Ok(())
}

/// The retried copy leaves the nursery partly used — and *usable*: the next, larger payload is
/// refused again, collects, and is delivered. The two messages are whole and in FIFO order.
#[test]
fn the_collection_leaves_the_heap_usable_for_a_second_delivery() -> Result<()> {
    let mut receiver = Process::new(pid(3))?;
    let mut src = source();
    let from = pid(4).to_eterm();

    let capacity = fill_nursery(&mut receiver);
    let grown = next_heap_size(capacity, 1);

    // Round 1: the payload only fits after the retry's collection (see the test above).
    let first_words = grown / 2;
    let first_elems = tuple_elems(first_words);
    let first = src
        .tuple(&first_elems)
        .expect("the first payload fits the source heap");
    receiver.deliver(from, first, &OffHeapTable::new())?;
    assert_eq!(receiver.mailbox().len(), 1);
    assert_eq!(receiver.heap_used_words(), first_words);
    assert_eq!(
        receiver.collector().minor_gcs(),
        1,
        "round 1's refused copy collected exactly once"
    );

    // Round 2: only `grown - first_words` words are free, so this payload is refused too and takes
    // the same collect-and-retry path. The retry succeeds because the collection resizes the
    // nursery from what the round used (`HeapPolicy::young_after_collection`), which keeps the
    // capacity the refused payload needs — the collection left a usable heap, not a wedged one.
    let second_words = grown * 3 / 4;
    assert!(
        second_words > grown - first_words,
        "the second payload must not fit the space left by the first"
    );
    assert!(
        second_words <= grown,
        "and it must fit the nursery that survives the collect"
    );
    let second_elems = tuple_elems(second_words);
    let second = src
        .tuple(&second_elems)
        .expect("the second payload fits the source heap");
    let outcome = receiver.deliver(from, second, &OffHeapTable::new())?;
    assert!(matches!(
        outcome,
        DeliverOutcome::Enqueued | DeliverOutcome::Woke
    ));
    assert_eq!(
        receiver.collector().minor_gcs(),
        2,
        "round 2's refused copy collected exactly once more -- the retry path was taken again"
    );

    // One message per delivery -- no duplicates.
    assert_eq!(receiver.mailbox().len(), 2);

    // ... and both are whole, in FIFO order.
    let first_copied = receiver
        .receive(|_| true)
        .expect("the first message")
        .payload();
    assert_eq!(size_object(first_copied), first_words);
    let expected_first = src
        .tuple(&first_elems)
        .expect("the expected first tuple fits the source heap");
    assert!(compare::eq(first_copied, expected_first, &AtomTable::new()));

    let second_copied = receiver
        .receive(|_| true)
        .expect("the second message")
        .payload();
    assert_eq!(size_object(second_copied), second_words);
    let expected_second = src
        .tuple(&second_elems)
        .expect("the expected second tuple fits the source heap");
    assert!(compare::eq(second_copied, expected_second, &AtomTable::new()));
    assert!(receiver.receive(|_| true).is_none(), "exactly two messages");

    Ok(())
}

/// A payload that genuinely cannot fit is the typed allocation error — the retry is a *retry*, not
/// a fallback into a fragment — and neither the mailbox nor the heap is touched.
///
/// The bound is made deterministic by the heap policy: with `max_heap_words == capacity` the
/// nursery can never grow past its initial capacity, so a payload one word larger than the whole
/// nursery is refused on the first copy, survives the collection, and is refused again by the
/// retry.
#[test]
fn a_payload_that_cannot_fit_is_a_typed_error_and_leaves_the_mailbox_untouched() -> Result<()> {
    let capacity = HeapPolicy::default().initial_young_words;
    let bounded = HeapPolicy::from_min_max(capacity, capacity);
    let mut receiver = Process::with_policy_and_priority(pid(5), bounded, Priority::Normal)?;
    let mut src = source();
    let from = pid(6).to_eterm();

    assert_eq!(receiver.collector().young().capacity_words(), capacity);
    assert_eq!(receiver.collector().young().available_words(), capacity);
    assert_eq!(receiver.heap_used_words(), 0);

    // One word past the whole nursery: unfittable under this policy, before AND after the collect.
    let oversized_words = capacity + 1;
    let oversized_elems = tuple_elems(oversized_words);
    let payload = src
        .tuple(&oversized_elems)
        .expect("the oversized payload fits the source heap");
    assert_eq!(size_object(payload), oversized_words);

    match receiver.deliver(from, payload, &OffHeapTable::new()) {
        Err(BeamError::Alloc(failure)) => {
            assert_eq!(
                failure.kind,
                AllocKind::Message,
                "a payload refused by the destination heap is the message-heap exhaustion"
            );
            assert_eq!(
                failure.requested,
                oversized_words * core::mem::size_of::<Eterm>(),
                "the failure reports the words the copy still wanted"
            );
        }
        other => panic!("expected the typed allocation failure, got {other:?}"),
    }

    // No partial write and no phantom message: the copier preflights before writing a word, and
    // the recovery path never enqueues.
    assert_eq!(
        receiver.collector().minor_gcs(),
        1,
        "the refused copy did collect once -- the retry was attempted, not skipped"
    );
    assert_eq!(receiver.mailbox().len(), 0, "a refused payload is not enqueued");
    assert_eq!(
        receiver.heap_used_words(),
        0,
        "the failed copy wrote nothing (capacity is preflighted)"
    );
    assert_eq!(
        receiver.collector().young().capacity_words(),
        capacity,
        "the bounded policy held: the collection could not grow the nursery"
    );

    // The bound is the nursery's size, not the retry: a payload that fits is delivered normally,
    // so the process is not left in a wedged state.
    let fitting_words = capacity / 2;
    let fitting_elems = tuple_elems(fitting_words);
    let fitting = src
        .tuple(&fitting_elems)
        .expect("the fitting payload fits the source heap");
    let outcome = receiver.deliver(from, fitting, &OffHeapTable::new())?;
    assert!(matches!(
        outcome,
        DeliverOutcome::Enqueued | DeliverOutcome::Woke
    ));
    assert_eq!(receiver.mailbox().len(), 1);
    let copied = receiver.mailbox().peek().expect("one message").payload();
    assert_eq!(size_object(copied), fitting_words);
    let expected = src
        .tuple(&fitting_elems)
        .expect("the expected tuple fits the source heap");
    assert!(compare::eq(copied, expected, &AtomTable::new()));

    Ok(())
}
