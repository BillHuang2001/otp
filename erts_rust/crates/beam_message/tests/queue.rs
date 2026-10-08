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

//! Integration tests for the data-level messaging primitives ([`send`](beam_message::send) /
//! [`receive`](beam_message::receive) / [`receive_ref`](beam_message::receive_ref)).
//!
//! Also pins the **atomicity** of `send`: a rejected send (a payload that does not fit the receiver
//! heap) must be a total no-op: the receiver heap, the destination off-heap table and the queue are
//! all left exactly as they were, so a host can safely retry through
//! [`send_fragmented`](beam_message::send_fragmented).

use beam_alloc::Heap;
use beam_common::{AllocKind, BeamError};
use beam_message::off_heap::OffHeapTable;
use beam_message::{receive, receive_ref, send, CopyContext, MsoHandle, Queue, QueueCursor};
use beam_term::prelude::*;
use beam_term::compare;

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first, synchronously (no recursion).
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the list fits the source heap");
    }
    acc
}

/// Builds the off-heap reference container `[HEADER_BIN_REF, slot, 0]` on `heap`.
fn bin_ref(heap: &mut Heap, slot: usize) -> Eterm {
    // SAFETY: the heap owns these three words.
    unsafe {
        let p = heap.alloc_words(3).expect("the container fits");
        *p = HEADER_BIN_REF;
        *p.add(1) = Eterm::from_raw(slot);
        *p.add(2) = Eterm::from_raw(0);
        make_boxed(p as *const Eterm)
    }
}

/// The first element of a copied payload list.
fn head(term: Eterm) -> i64 {
    // SAFETY: every payload built by these tests is a non-empty cons list.
    unsafe { signed_value(car(term)) }
}

#[test]
fn send_copies_into_the_receiver_heap_and_enqueues() {
    let mut src = Heap::new(64).expect("source heap");
    let term = build_list(&mut src, &[make_small(1), make_small(2), make_small(3)]);
    let from = make_small(7);

    let mut to_heap = Heap::new(64).expect("receiver heap");
    let mut queue = Queue::new();
    let src_off_heap = OffHeapTable::new();
    let mut dst_off_heap = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);

    send(from, term, &mut to_heap, &mut queue, &mut ctx).expect("the send succeeds");

    assert_eq!(queue.len(), 1);
    assert!(ctx.dst_off_heap().is_empty(), "send drains the destination table");

    let message = queue.peek().expect("one queued message");
    assert_eq!(message.from(), from);
    assert!(message.off_heap().is_empty(), "no off-heap objects were referenced");
    // The payload is a genuine copy on the receiver heap, not the sender's term.
    assert_ne!(list_val(message.payload()) as usize, list_val(term) as usize);
    assert_eq!(to_heap.used(), 6, "three cons cells");
    assert!(compare::eq(term, message.payload(), &AtomTable::new()));
}

#[test]
fn three_sends_dequeue_in_fifo_order() {
    let mut src = Heap::new(64).expect("source heap");
    let from = make_small(0);

    let mut to_heap = Heap::new(64).expect("receiver heap");
    let mut queue = Queue::new();
    let src_off_heap = OffHeapTable::new();
    let mut dst_off_heap = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);

    for value in [1i64, 2, 3] {
        let term = build_list(&mut src, &[make_small(value)]);
        send(from, term, &mut to_heap, &mut queue, &mut ctx).expect("the send succeeds");
    }
    assert_eq!(queue.len(), 3);

    assert_eq!(queue.dequeue().map(|m| head(m.payload())), Some(1));
    assert_eq!(queue.dequeue().map(|m| head(m.payload())), Some(2));
    assert_eq!(queue.dequeue().map(|m| head(m.payload())), Some(3));
    assert!(queue.dequeue().is_none());
}

#[test]
fn receive_selects_and_removes_only_the_first_match() {
    let mut src = Heap::new(64).expect("source heap");
    let from = make_small(0);

    let mut to_heap = Heap::new(256).expect("receiver heap");
    let mut queue = Queue::new();
    let src_off_heap = OffHeapTable::new();
    let mut dst_off_heap = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);

    for value in [1i64, 2, 3] {
        let term = build_list(&mut src, &[make_small(value)]);
        send(from, term, &mut to_heap, &mut queue, &mut ctx).expect("the send succeeds");
    }
    assert_eq!(queue.len(), 3);

    let got = receive(&mut queue, |m| head(m.payload()) == 2);
    assert_eq!(got.map(|m| head(m.payload())), Some(2));
    assert_eq!(queue.len(), 2);
    assert_eq!(
        queue.iter().map(|m| head(m.payload())).collect::<Vec<_>>(),
        vec![1, 3],
        "the non-matching messages stay queued in order"
    );

    // No match leaves the queue untouched.
    assert!(receive(&mut queue, |m| head(m.payload()) == 9).is_none());
    assert_eq!(queue.len(), 2);

    // `receive_ref` borrows without removing.
    let borrowed = receive_ref(&queue, |m| head(m.payload()) == 3);
    assert_eq!(borrowed.map(|m| head(m.payload())), Some(3));
    assert_eq!(queue.len(), 2);
    assert!(receive_ref(&queue, |m| head(m.payload()) == 9).is_none());
}

#[test]
fn send_attaches_the_referenced_off_heap_objects_to_the_message() {
    let mut src_off_heap = OffHeapTable::new();
    let slot = src_off_heap.push(beam_gc::Mso::new(0x1234u32, 8));

    let mut src = Heap::new(16).expect("source heap");
    // SAFETY: the heap owns these three words: the off-heap reference container.
    let container = unsafe {
        let p = src.alloc_words(3).expect("the bin-ref fits");
        *p = HEADER_BIN_REF;
        *p.add(1) = Eterm::from_raw(slot);
        *p.add(2) = Eterm::from_raw(0);
        make_boxed(p as *const Eterm)
    };

    let mut to_heap = Heap::new(16).expect("receiver heap");
    let mut queue = Queue::new();
    let mut dst_off_heap = OffHeapTable::new();
    {
        let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);
        send(make_small(0), container, &mut to_heap, &mut queue, &mut ctx)
            .expect("the send succeeds");
    }

    let message = queue.peek().expect("one queued message");
    assert_eq!(message.off_heap().len(), 1);
    assert_eq!(message.off_heap()[0].refcount(), 2, "source + message");
    assert_eq!(
        message.off_heap()[0].downcast_ref::<u32>(),
        Some(&0x1234),
        "the payload's off-heap object was refcounted, not deep-copied"
    );
    assert!(
        dst_off_heap.is_empty(),
        "send drained the destination table into the message"
    );
}

#[test]
fn cursor_scans_candidates_and_removes_the_match() {
    // The engine's receive path: scan candidate by candidate with a cursor, advance on a miss,
    // remove the match, then reset to the first message.
    let mut src = Heap::new(64).expect("source heap");
    let from = make_small(0);

    let mut to_heap = Heap::new(256).expect("receiver heap");
    let mut queue = Queue::new();
    let src_off_heap = OffHeapTable::new();
    let mut dst_off_heap = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);

    for value in [10i64, 20, 30, 40] {
        let term = build_list(&mut src, &[make_small(value)]);
        send(from, term, &mut to_heap, &mut queue, &mut ctx).expect("the send succeeds");
    }

    assert_eq!(queue.len(), 4);
    assert_eq!(queue.position(QueueCursor::default()), 0, "the default cursor is first");
    assert_eq!(queue.position(queue.cursor()), 0);

    // Walk the queue with a cursor until the matching payload is found.
    let mut cursor = queue.cursor();
    let mut match_cursor = None;
    while let Some(message) = queue.message_at(cursor) {
        if head(message.payload()) == 30 {
            match_cursor = Some(cursor);
            break;
        }
        queue.advance(&mut cursor);
    }

    let removed = queue.remove_at(match_cursor.expect("the match is queued"));
    assert_eq!(removed.map(|m| head(m.payload())), Some(30));
    assert_eq!(queue.len(), 3);
    assert_eq!(
        queue.iter().map(|m| head(m.payload())).collect::<Vec<_>>(),
        vec![10, 20, 40],
        "the scanned-over messages stay queued in front of the match"
    );

    // A cursor at/after the shrunken queue's end is past-the-end and removes nothing.
    let stale = queue.cursor_at(3);
    assert!(queue.message_at(stale).is_none());
    assert!(queue.remove_at(stale).is_none());
    assert_eq!(queue.len(), 3);

    // Resetting to the first message (the engine's post-remove reset) sees the head again.
    let fresh = queue.cursor();
    assert_eq!(
        queue.message_at(fresh).map(|m| head(m.payload())),
        Some(10)
    );

    // A cursor over an empty queue is past-the-end.
    queue.clear();
    let empty = queue.cursor();
    assert!(queue.message_at(empty).is_none());
    assert!(queue.remove_at(empty).is_none());
    assert!(queue.is_empty());
}

#[test]
fn a_rejected_send_is_a_total_no_op() {
    // The payload's graph references an off-heap MSO object AND is far larger than the receiver heap
    // (so the copier's up-front capacity preflight rejects it before it writes a word).
    let mut src_off_heap = OffHeapTable::new();
    assert_eq!(src_off_heap.push(beam_gc::Mso::new(0x1234u32, 8)), 0);

    let mut src = Heap::new(64).expect("source heap");
    let container = bin_ref(&mut src, 0);
    let nested = src
        .tuple(&[container, make_small(1), make_small(2)])
        .expect("the tuple fits the source heap");
    let term = build_list(&mut src, &[nested, make_small(9), make_small(10)]);

    let mut to_heap = Heap::new(2).expect("a receiver heap far too small");
    let mut queue = Queue::new();
    let mut dst_off_heap = OffHeapTable::new();
    {
        let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);
        match send(make_small(0), term, &mut to_heap, &mut queue, &mut ctx) {
            Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Message),
            other => panic!("expected a message allocation failure, got {other:?}"),
        }
    }

    // Nothing changed anywhere: the receiver heap never grew, the destination off-heap table holds
    // neither the MSO object nor a binary/node, the queue is still empty, and the source's
    // off-heap object was not refcounted.
    assert_eq!(to_heap.used(), 0, "the rejected copy wrote no word");
    assert_eq!(to_heap.available(), to_heap.capacity(), "the whole receiver heap is still free");
    assert!(dst_off_heap.is_empty(), "no MSO object was registered");
    assert!(dst_off_heap.binaries().is_empty(), "no refc binary was registered");
    assert!(dst_off_heap.nodes().is_empty(), "no node was registered");
    assert!(queue.is_empty(), "nothing was enqueued");
    assert_eq!(
        src_off_heap.get(0).map(MsoHandle::refcount),
        Some(1),
        "the source's off-heap object was not refcounted"
    );
}
