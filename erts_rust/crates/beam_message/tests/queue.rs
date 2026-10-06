//! Integration tests for the data-level messaging primitives ([`send`](beam_message::send) /
//! [`receive`](beam_message::receive) / [`receive_ref`](beam_message::receive_ref)).

use beam_alloc::Heap;
use beam_message::off_heap::OffHeapTable;
use beam_message::{receive, receive_ref, send, CopyContext, Queue, QueueCursor};
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
