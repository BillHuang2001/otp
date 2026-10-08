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

//! Integration evidence for [`Process::heap_and_off_heap_mut`] and
//! [`Process::heap_and_mailbox_mut`] — the split borrows that let a caller hold the young
//! (mutator) heap alongside the off-heap (MSO) table, or alongside the mailbox.
//!
//! `Process::heap_mut`, `Process::off_heap_mut` and `Process::mailbox_mut` each take a whole
//! `&mut self`, so no caller can hold any two of them. This suite proves the split accessors hand
//! out the process's **own** storage (not copies): writing through the returned references is
//! observed through the `Process`'s own accessors, the references are pointer-identical to what
//! the whole-`self` accessors return, and the motivating consumers — [`beam_message::Fragments::move_into`]
//! (heap + off-heap table) and [`beam_message::send_fragmented`] (heap + mailbox) — land their
//! payload on the process's real young heap and real mailbox.

use beam_alloc::Heap;
use beam_gc::Mso;
use beam_message::{
    send_fragmented, CopyContext, Fragments, HeapFragment, Message, OffHeapTable, Queue,
};
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{make_small, signed_value, AtomTable, NIL};

/// A pid with the given number and serial 0.
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

#[test]
fn heap_and_off_heap_mut_hands_out_the_processes_own_storage() {
    let mut process = Process::new(pid(1)).expect("the default heap allocates");
    assert_eq!(
        process.heap_used_words(),
        0,
        "a fresh process has an empty young heap"
    );
    assert!(
        process.off_heap().is_empty(),
        "a fresh process has an empty off-heap table"
    );

    // A single call that hands out BOTH references at once — the whole reason the accessor
    // exists. Write through the heap reference (allocate a term) and through the off-heap
    // reference (insert an entry), then capture the addresses handed out.
    let (heap_addr, off_heap_addr) = {
        let (heap, off_heap) = process.heap_and_off_heap_mut();

        // Write through the heap reference.
        let _term = heap
            .tuple(&[make_small(1), make_small(2)])
            .expect("the tuple fits the young heap");
        assert_eq!(
            heap.used(),
            3,
            "the tuple wrote 3 words through this reference"
        );

        // Write through the off-heap reference.
        let slot = off_heap.push(Mso::new(0xABC_Du32, 8));
        assert_eq!(slot, 0, "the first off-heap slot is 0");
        assert_eq!(
            off_heap.len(),
            1,
            "the insert landed in this reference's table"
        );

        (heap as *mut Heap, off_heap as *mut OffHeapTable)
    };

    // The `Process` itself observes BOTH writes through its own accessors: the words are on its
    // young heap and the off-heap slot is in its own table.
    assert_eq!(
        process.heap_used_words(),
        3,
        "the write through the heap reference landed on the process's OWN young heap"
    );
    assert_eq!(
        process.off_heap().len(),
        1,
        "the write through the off-heap reference landed on the process's OWN table"
    );
    assert_eq!(
        process
            .off_heap()
            .get(0)
            .and_then(|handle| handle.downcast_ref::<u32>())
            .copied(),
        Some(0xABC_D),
        "the entry the split reference inserted is looked up through the process's own accessor"
    );

    // The references are the VERY SAME objects `heap_mut`/`off_heap_mut` return — not copies of
    // the storage. This rules out an implementation that handed back detached temporaries.
    assert!(
        core::ptr::eq(process.heap_mut() as *mut Heap, heap_addr),
        "the returned heap reference is the process's own young heap (same address)"
    );
    assert!(
        core::ptr::eq(process.off_heap_mut() as *mut OffHeapTable, off_heap_addr),
        "the returned off-heap reference is the process's own table (same address)"
    );
}

#[test]
fn fragments_move_into_moves_onto_the_processes_own_heap() {
    // The motivating consumer of the split accessor: build a fragmented payload whose root points
    // into a *separate* fragment heap (the sender-side representation of an oversized message).
    let mut src = Heap::new(16).expect("the source heap allocates");
    let payload = {
        // The list `[1, 2, 3]`, tail-first.
        let tail = src.cons(make_small(3), NIL).expect("a cons fits");
        let mid = src.cons(make_small(2), tail).expect("a cons fits");
        src.cons(make_small(1), mid).expect("a cons fits")
    };
    let src_off_heap = OffHeapTable::new();
    let mut fragment = HeapFragment::new(16).expect("the fragment allocates");
    fragment
        .copy_root(payload, &src_off_heap)
        .expect("the payload fits the fragment");
    let mut chain = Fragments::new();
    chain.push(fragment);

    let mut process = Process::new(pid(1)).expect("the default heap allocates");
    let used_before = process.heap_used_words();

    // `move_into` needs the destination heap AND the destination off-heap table *simultaneously* —
    // the exact pair the split accessor hands out of one `&mut process`.
    let moved = {
        let (heap, off_heap) = process.heap_and_off_heap_mut();
        chain
            .move_into(heap, off_heap)
            .expect("the payload moves onto the receiver's heap")
    };

    assert_eq!(moved.len(), 1, "the single fragment root moved");
    // The three cons cells (six words) landed on the RECEIVER's young heap: the process observes
    // exactly that growth through its own accessor.
    assert_eq!(
        process.heap_used_words(),
        used_before + 6,
        "the moved list lives on the process's own young heap (3 cons cells = 6 words)"
    );
    assert!(
        compare::eq(payload, moved[0], &AtomTable::new()),
        "the moved term is structurally equal to the source payload"
    );
}

#[test]
fn heap_and_mailbox_mut_hands_out_the_processes_own_storage() {
    let mut process = Process::new(pid(2)).expect("the default heap allocates");
    assert_eq!(
        process.heap_used_words(),
        0,
        "a fresh process has an empty young heap"
    );
    assert_eq!(
        process.mailbox().len(),
        0,
        "a fresh process has an empty mailbox"
    );

    // A single call that hands out BOTH references at once. Write through the heap reference
    // (allocate a term) and through the mailbox reference (enqueue a message), then capture the
    // addresses handed out.
    let (heap_addr, mailbox_addr) = {
        let (heap, mailbox) = process.heap_and_mailbox_mut();

        // Write through the heap reference.
        let _term = heap
            .tuple(&[make_small(1), make_small(2)])
            .expect("the tuple fits the young heap");
        assert_eq!(
            heap.used(),
            3,
            "the tuple wrote 3 words through this reference"
        );

        // Write through the mailbox reference.
        mailbox.enqueue(Message::new(make_small(42), make_small(0)));
        assert_eq!(
            mailbox.len(),
            1,
            "the message landed in this reference's queue"
        );

        (heap as *mut Heap, mailbox as *mut Queue)
    };

    // The `Process` itself observes BOTH writes through its own accessors: the words are on its
    // young heap and the message is in its own mailbox.
    assert_eq!(
        process.heap_used_words(),
        3,
        "the write through the heap reference landed on the process's OWN young heap"
    );
    assert_eq!(
        process.mailbox().len(),
        1,
        "the message enqueued through the mailbox reference landed in the process's OWN mailbox"
    );
    assert_eq!(
        process.mailbox().peek().map(|m| signed_value(m.payload())),
        Some(42),
        "the message the split reference enqueued is looked up through the process's own accessor"
    );

    // The references are the VERY SAME objects `heap_mut`/`mailbox_mut` return — not copies of
    // the storage. This rules out an implementation that handed back detached temporaries.
    assert!(
        core::ptr::eq(process.heap_mut() as *mut Heap, heap_addr),
        "the returned heap reference is the process's own young heap (same address)"
    );
    assert!(
        core::ptr::eq(process.mailbox_mut() as *mut Queue, mailbox_addr),
        "the returned mailbox reference is the process's own queue (same address)"
    );
}

#[test]
fn send_fragmented_through_the_split_borrow_lands_in_the_processes_own_mailbox() {
    // The motivating consumer of the `(heap, mailbox)` split: `send_fragmented` copies a payload
    // onto the receiver's heap **and** enqueues the resulting message on the receiver's queue, so
    // it needs both at once. A source heap holds the payload `[1, 2, 3]`; the copy boundary
    // resolves off-heap references through `src_off_heap` and registers them in `dst_off_heap`.
    let mut src = Heap::new(16).expect("the source heap allocates");
    let payload = {
        // The list `[1, 2, 3]`, tail-first.
        let tail = src.cons(make_small(3), NIL).expect("a cons fits");
        let mid = src.cons(make_small(2), tail).expect("a cons fits");
        src.cons(make_small(1), mid).expect("a cons fits")
    };
    let src_off_heap = OffHeapTable::new();
    let mut dst_off_heap = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);

    let mut process = Process::new(pid(3)).expect("the default heap allocates");
    let used_before = process.heap_used_words();

    {
        // Obtain `(heap, mailbox)` from one `&mut process` — the pair is otherwise unobtainable
        // because `heap_mut()` and `mailbox_mut()` each take a whole `&mut self`.
        let (heap, mailbox) = process.heap_and_mailbox_mut();
        send_fragmented(make_small(7), payload, heap, mailbox, &mut ctx, 0)
            .expect("the small payload fits the receiver's young heap");
    }

    // A payload that fits takes the ordinary (non-fragment) `send` branch: the message landed in
    // the process's OWN mailbox, carrying the copied payload that lives on the process's OWN
    // young heap.
    assert_eq!(
        process.mailbox().len(),
        1,
        "the message landed in the process's OWN mailbox"
    );
    let message = process.mailbox().peek().expect("the queue is non-empty");
    assert_eq!(
        signed_value(message.from()),
        7,
        "the sender identity handed to `send_fragmented` was recorded on the message"
    );
    assert!(
        message.fragments().is_none(),
        "a payload that fits the receiver's heap uses no heap fragment"
    );
    assert!(
        compare::eq(payload, message.payload(), &AtomTable::new()),
        "the enqueued payload is structurally equal to the source list"
    );
    // The payload's 3 cons cells (6 words) were copied onto the RECEIVER's own young heap.
    assert_eq!(
        process.heap_used_words(),
        used_before + 6,
        "the copied list lives on the process's own young heap (3 cons cells = 6 words)"
    );
}

#[test]
fn send_fragmented_falls_back_to_a_fragment_within_the_processes_own_mailbox() {
    // The same accessor drives `send_fragmented`'s fragment branch: a payload far larger than the
    // receiver's young heap makes `copy_struct` fail with `AllocKind::Message`, so the message must
    // be routed through a heap fragment — still landing in the process's OWN mailbox.
    let mut src = Heap::new(4096).expect("the source heap allocates");
    let mut payload = NIL;
    for value in 0..1000 {
        payload = src
            .cons(make_small(value), payload)
            .expect("a cons fits the source heap");
    }
    let src_off_heap = OffHeapTable::new();
    let mut dst_off_heap = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);

    let mut process = Process::new(pid(4)).expect("the default heap allocates");
    let used_before = process.heap_used_words();

    {
        let (heap, mailbox) = process.heap_and_mailbox_mut();
        send_fragmented(make_small(9), payload, heap, mailbox, &mut ctx, 0)
            .expect("the oversized payload is routed through a heap fragment");
    }

    assert_eq!(
        process.mailbox().len(),
        1,
        "the fragmented message landed in the process's OWN mailbox"
    );
    let message = process.mailbox().peek().expect("the queue is non-empty");
    assert_eq!(
        signed_value(message.from()),
        9,
        "the sender identity was recorded"
    );
    assert!(
        message.fragments().is_some(),
        "a payload too large for the receiver's heap is routed through a heap fragment"
    );
    assert_eq!(
        process.heap_used_words(),
        used_before,
        "the fragment — not the young heap — holds the oversized payload"
    );
    assert!(
        compare::eq(payload, message.payload(), &AtomTable::new()),
        "the fragmented payload is structurally equal to the source list"
    );
}
