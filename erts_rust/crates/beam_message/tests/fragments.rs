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

//! Integration tests for the **message-fragment model** ([`HeapFragment`] / [`Fragments`] /
//! [`send_fragmented`](beam_message::send_fragmented)).
//!
//! Covers the required evidence of the crate `CONTEXT.md` `Test Strategy`:
//!
//! 1. a payload larger than the receiver heap is **fragmented**: plain `send` fails with the typed
//!    message allocation failure and leaves the receiver heap untouched, while
//!    [`send_fragmented`](beam_message::send_fragmented) enqueues a message whose payload points
//!    into the fragment and is structurally equal to the source,
//! 2. [`Fragments::move_into`] copies the payload onto the receiver heap (the move analogue of
//!    `erts_move_multi_frags`/`erts_copy_one_frag`) and transfers the fragment's off-heap objects,
//! 3. a **multi-fragment** chain moves every root back in order,
//! 4. a fragmented message carrying a `RefcBinary` moves the **handle** (same `data_ptr`, the
//!    payload never deep-copied, refcount incremented), and one carrying an **external thing** moves
//!    its **node reference** (the receiver's table holds the entry the fragment released),
//! 5. fragment accounting (`used`/`capacity`/`total_used_words`/`off_heap_overhead_bytes`),
//! 6. the **oversized-message path is atomic**: a rejected [`send`] and a rejected
//!    [`HeapFragment::copy_root`] each write nothing (heap, off-heap table and queue unchanged), and
//!    an accepted [`send_fragmented`] uses exactly **one** fragment sized to the whole payload,
//! 7. fragmented payloads of several shapes (a refc **binary**, a **HAMT map** (>32 keys) and a
//!    **closure** built with `make_fun3` and boxed free variables) each move off the fragment onto
//!    the receiver heap and stay `=:=` their source,
//! 8. [`Fragments::move_into`] gives up **no** fragment's off-heap ownership until the whole chain
//!    has moved: a mid-move failure leaves the chain intact.

use beam_alloc::{Heap, RefcBinary};
use beam_common::{AllocKind, BeamError};
use beam_gc::Mso;
use beam_message::off_heap::OffHeapTable;
use beam_message::{
    send, send_fragmented, size_of_copy, CopyContext, Fragments, HeapFragment, MsoHandle, Queue,
};
use beam_term::compare;
use beam_term::prelude::*;

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first (no recursion).
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the list fits the source heap");
    }
    acc
}

/// The address of `term`'s object (its first word), or `None` for a non-compound term.
fn object_addr(term: Eterm) -> Option<usize> {
    if is_list(term) {
        Some(list_val(term) as usize)
    } else if is_boxed(term) {
        Some(boxed_val(term) as usize)
    } else {
        None
    }
}

/// Whether `term`'s object lies inside `heap`'s (non-reallocating) region.
fn on_heap(heap: &Heap, term: Eterm) -> bool {
    match object_addr(term) {
        Some(addr) => {
            // `hend - capacity` is the region base (`Heap` never reallocates).
            let base = heap.hend().wrapping_sub(heap.capacity()) as usize;
            let end = heap.hend() as usize;
            addr >= base && addr < end
        }
        None => false,
    }
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

/// Writes the on-heap refc-binary pair for `data_ptr`/`len` and returns the tagged root (the
/// `ErlSubBits`).
///
/// # Safety
///
/// `heap` must have 8 spare words and `data_ptr` must be a live, `UNIT_SIZE`-aligned payload of
/// `len` bytes that outlives the returned term.
unsafe fn refc_binary_term(heap: &mut Heap, data_ptr: *mut u8, len: usize) -> Eterm {
    // SAFETY: the caller keeps the payload alive; `data_ptr` is `UNIT_SIZE`-aligned.
    unsafe {
        let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).expect("the pair fits");
        write_refc_binary(
            hp,
            data_ptr as *mut Eterm,
            core::ptr::null_mut(),
            data_ptr,
            0,
            len * 8,
        )
    }
}

#[test]
fn a_payload_larger_than_the_receiver_heap_is_fragmented() {
    let mut src = Heap::new(128).expect("source heap");
    let elems: Vec<Eterm> = (0..20).map(make_small).collect();
    let term = build_list(&mut src, &elems);
    let from = make_small(7);

    let mut to_heap = Heap::new(4).expect("a receiver heap far too small for the payload");
    let mut queue = Queue::new();
    let src_off = OffHeapTable::new();
    let mut dst_off = OffHeapTable::new();

    // 1. Plain `send` fails with the typed message allocation failure and is a total no-op: the
    // receiver heap, the destination off-heap table and the queue are all untouched.
    {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        match send(from, term, &mut to_heap, &mut queue, &mut ctx) {
            Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Message),
            other => panic!("expected a message allocation failure, got {other:?}"),
        }
    }
    assert_eq!(to_heap.used(), 0, "the failed copy wrote nothing");
    assert_eq!(to_heap.available(), to_heap.capacity(), "the whole heap is still free");
    assert!(dst_off.is_empty(), "no off-heap object was registered by the failed copy");
    assert!(queue.is_empty(), "nothing was enqueued by the failed send");

    // 2. The SAME context/queue then succeeds through `send_fragmented`: the failed attempt left no
    // partial state behind, so the retry needs no cleanup (the host's `deliver_message` retry).
    {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        send_fragmented(from, term, &mut to_heap, &mut queue, &mut ctx, size_of_copy(term))
            .expect("the fragmented send succeeds");
    }
    assert_eq!(to_heap.used(), 0, "the payload lives in the fragment, not the receiver heap");
    assert!(dst_off.is_empty(), "the fragment path does not use the receiver's off-heap table");

    let message = queue.peek().expect("one queued message");
    assert_eq!(message.from(), from);
    assert!(
        compare::eq(term, message.payload(), &AtomTable::new()),
        "the fragment-resident payload is structurally equal to the source"
    );

    let fragment = message
        .fragments()
        .expect("a fragmented message reports its fragments");
    assert_eq!(fragment.len(), 1);
    let only = fragment.iter().next().expect("one fragment");
    assert!(only.contains(message.payload()), "the payload points into the fragment");
    assert!(
        object_addr(message.payload()) != object_addr(term),
        "the payload is a different address from the source"
    );
    assert!(!on_heap(&to_heap, message.payload()), "and not in the receiver heap");
    assert_eq!(only.used(), size_of_copy(term), "the whole payload is in the fragment");
    // A fragmented message carries its off-heap objects in the fragment, not on the envelope.
    assert!(message.off_heap().is_empty());
    assert!(message.binaries().is_empty());
    assert!(message.nodes().is_empty());
}

#[test]
fn move_into_moves_the_payload_onto_the_receiver_heap_and_transfers_off_heap() {
    const PAYLOAD: u32 = 0x0BAD_F00D;

    // The payload references an off-heap MSO object through a container.
    let mut src_off = OffHeapTable::new();
    assert_eq!(src_off.push(Mso::new(PAYLOAD, 8)), 0);

    let mut src = Heap::new(32).expect("source heap");
    let container = bin_ref(&mut src, 0);
    let term = src
        .tuple(&[container, make_small(99)])
        .expect("the tuple fits the source heap");
    let from = make_small(0);

    let mut to_heap = Heap::new(1).expect("too small for the payload");
    let mut queue = Queue::new();
    {
        let mut dst_off = OffHeapTable::new();
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        send_fragmented(from, term, &mut to_heap, &mut queue, &mut ctx, size_of_copy(term))
            .expect("the fragmented send succeeds");
    }
    assert_eq!(src_off.get(0).map(MsoHandle::refcount), Some(2), "source + fragment");

    let mut message = queue.dequeue().expect("the fragmented message");
    let fragment_root = message.payload();
    assert!(message.fragments().expect("fragmented").iter().next().unwrap().contains(fragment_root));

    let mut receiver_heap = Heap::new(32).expect("receiver heap");
    let mut receiver_off = OffHeapTable::new();
    let moved = message
        .fragments_mut()
        .expect("fragmented")
        .move_into(&mut receiver_heap, &mut receiver_off)
        .expect("the move succeeds");

    assert_eq!(moved.len(), 1);
    assert!(on_heap(&receiver_heap, moved[0]), "the moved root lives on the receiver heap");
    assert!(!on_heap(&receiver_heap, fragment_root), "the fragment root is elsewhere");

    // The off-heap object was transferred: the destination table holds it.
    assert_eq!(receiver_off.len(), 1, "the off-heap object moved into the destination table");
    assert_eq!(receiver_off.get(0).and_then(|h| h.downcast_ref::<u32>()), Some(&PAYLOAD));
    assert_eq!(src_off.get(0).map(MsoHandle::refcount), Some(2), "source + receiver");
    assert!(
        message.fragments().expect("fragmented").iter().next().unwrap().off_heap().is_empty(),
        "the fragment gave up its own references"
    );

    // The moved container resolves to the destination slot; the small int is unchanged.
    // SAFETY: `moved[0]` is a live 2-tuple.
    let moved_container = unsafe { tuple_element(moved[0], 0) };
    assert!(is_boxed(moved_container));
    // SAFETY: the moved container's header word is live, so word 1 is its `val`.
    assert_eq!(unsafe { *boxed_val(moved_container).add(1) }.raw(), 0);
    // SAFETY: `moved[0]` is a live 2-tuple.
    assert_eq!(unsafe { signed_value(tuple_element(moved[0], 1)) }, 99);
}

#[test]
fn a_multi_fragment_chain_moves_every_root_in_order() {
    let mut src = Heap::new(64).expect("source heap");
    let first = build_list(&mut src, &[make_small(1), make_small(2)]);
    let second = src
        .tuple(&[make_small(3), make_small(4)])
        .expect("the tuple fits the source heap");
    let src_off = OffHeapTable::new();

    // Two fragments, each with its own independent root, chained in order.
    let mut frag_a = HeapFragment::new(size_of_copy(first)).expect("fragment A");
    frag_a.copy_root(first, &src_off).expect("root A fits");
    let mut frag_b = HeapFragment::new(size_of_copy(second)).expect("fragment B");
    frag_b.copy_root(second, &src_off).expect("root B fits");
    assert!(frag_a.contains(frag_a.roots()[0]));
    assert!(frag_b.contains(frag_b.roots()[0]));

    let mut chain = Fragments::new();
    chain.push(frag_a);
    chain.push(frag_b);
    assert_eq!(chain.len(), 2);
    assert_eq!(chain.total_used_words(), size_of_copy(first) + size_of_copy(second));

    let mut dst = Heap::new(64).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let moved = chain.move_into(&mut dst, &mut dst_off).expect("the move succeeds");

    assert_eq!(moved.len(), 2, "both roots are moved back");
    assert!(compare::eq(first, moved[0], &AtomTable::new()));
    assert!(compare::eq(second, moved[1], &AtomTable::new()));
    assert!(on_heap(&dst, moved[0]) && on_heap(&dst, moved[1]), "both live on the destination heap");
    assert_eq!(dst.used(), size_of_copy(first) + size_of_copy(second));
    assert!(dst_off.is_empty(), "no off-heap objects were referenced");
}

#[test]
fn a_fragmented_message_shares_a_refc_binary_payload() {
    // A 256-byte payload whose bytes are position-dependent, so a deep/shifted copy is visible.
    let bytes: Vec<u8> = (0..256usize).map(|i| ((i * 7) % 251) as u8).collect();

    let mut src_off = OffHeapTable::new();
    src_off.push_binary(RefcBinary::new(&bytes).expect("the payload allocates"));
    let data_ptr = src_off.binaries()[0].data_ptr();
    assert_eq!(src_off.binaries()[0].refcount(), 1);

    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: `src_off` owns the payload, which outlives `term`.
    let term = unsafe { refc_binary_term(&mut src, data_ptr, bytes.len()) };

    let mut to_heap = Heap::new(4).expect("too small for the 8-word pair");
    let mut queue = Queue::new();
    {
        let mut dst_off = OffHeapTable::new();
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        send_fragmented(make_small(0), term, &mut to_heap, &mut queue, &mut ctx, size_of_copy(term))
            .expect("the fragmented send succeeds");
    }
    assert_eq!(to_heap.used(), 0);

    // The fragment owns a clone of the binary; its payload is shared, never deep-copied.
    let mut message = queue.dequeue().expect("the fragmented message");
    let only = message.fragments().expect("fragmented").iter().next().expect("one fragment");
    assert_eq!(only.off_heap().binaries().len(), 1);
    assert_eq!(only.off_heap().binaries()[0].data_ptr(), data_ptr);
    assert_eq!(only.off_heap().binaries()[0].as_bytes(), &bytes[..]);
    assert_eq!(src_off.binaries()[0].refcount(), 2, "source + fragment");

    let mut receiver_heap = Heap::new(64).expect("receiver heap");
    let mut receiver_off = OffHeapTable::new();
    let moved = message
        .fragments_mut()
        .expect("fragmented")
        .move_into(&mut receiver_heap, &mut receiver_off)
        .expect("the move succeeds");

    // The move copied the 8-word pair and moved the handle: SAME payload pointer, refcount up.
    assert_eq!(moved.len(), 1);
    assert_eq!(receiver_heap.used(), ERL_REFC_BITS_SIZE, "the whole pair is on the receiver heap");
    assert_eq!(receiver_off.binaries().len(), 1);
    let held = &receiver_off.binaries()[0];
    assert_eq!(held.data_ptr(), data_ptr, "the payload is shared, not deep-copied");
    assert_eq!(held.as_bytes(), &bytes[..], "the payload bytes are unchanged");
    assert_eq!(held.refcount(), 2, "source + receiver; the move's net refcount is unchanged");
    assert_eq!(src_off.binaries()[0].refcount(), 2);
    // The moved term is a well-formed refc binary over the same payload.
    assert!(unsafe { is_refc_binary(moved[0]) });
    assert_eq!(unsafe { refc_bytes(moved[0]) }, &bytes[..]);
}

#[test]
fn a_fragmented_message_transfers_an_external_things_node_reference() {
    let mut atoms = AtomTable::new();
    let node = make_atom(atoms.intern("nonode@nohost"));

    // The source process's heap holds an external pid; its node is registered by the copy, not
    // pre-registered in the source table.
    let mut src = Heap::new(32).expect("source heap");
    // SAFETY: the heap owns the 4-word external pid block.
    let term = unsafe {
        let hp = src.alloc_words(external_pid_size_words()).expect("the pid fits");
        write_external_pid(hp, node, 0x1234, 0x0007, 5)
    };
    let src_off = OffHeapTable::new();

    // A receiver heap too small for the 4-word block: the send must fragment.
    let mut to_heap = Heap::new(2).expect("too small for the external pid");
    let mut queue = Queue::new();
    {
        let mut dst_off = OffHeapTable::new();
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        send_fragmented(make_small(0), term, &mut to_heap, &mut queue, &mut ctx, 0)
            .expect("the fragmented send succeeds");
    }
    assert_eq!(to_heap.used(), 0);

    // The fragment's copy registered the node against the FRAGMENT's table (one reference).
    let mut message = queue.dequeue().expect("the fragmented message");
    let only = message.fragments().expect("fragmented").iter().next().expect("one fragment");
    assert_eq!(only.off_heap().nodes().len(), 1, "the fragment registered the node");
    assert_eq!(only.off_heap().nodes()[0].creation(), 5);
    let in_fragment = only.off_heap().nodes()[0].handle();
    assert_eq!(in_fragment.refcount(), 1);

    // Moving the payload onto the receiver heap hands the node reference over: the receiver's table
    // now holds a fresh entry for the identity, and the fragment released its own.
    let mut receiver_heap = Heap::new(32).expect("receiver heap");
    let mut receiver_off = OffHeapTable::new();
    let moved = message
        .fragments_mut()
        .expect("fragmented")
        .move_into(&mut receiver_heap, &mut receiver_off)
        .expect("the move succeeds");

    assert_eq!(moved.len(), 1);
    assert!(on_heap(&receiver_heap, moved[0]), "the copied thing is on the receiver heap");
    assert_eq!(receiver_heap.used(), external_pid_size_words(), "the 4-word block, verbatim");
    assert_eq!(receiver_off.nodes().len(), 1, "the receiver now owns the node reference");
    assert_eq!(receiver_off.nodes()[0].node().raw(), node.raw());
    assert_eq!(receiver_off.nodes()[0].creation(), 5);
    assert_eq!(receiver_off.nodes()[0].refcount(), 1);
    assert!(
        message
            .fragments()
            .expect("fragmented")
            .iter()
            .all(|f| f.off_heap().nodes().is_empty()),
        "every fragment gave its node reference up after the whole chain moved"
    );
    assert_eq!(in_fragment.refcount(), 0, "the fragment released its own entry");

    // The copied thing's words are preserved verbatim, node atom included.
    // SAFETY: `moved[0]` is a live 4-word external pid block.
    let (c_node, c_num, c_ser, c_creation) = unsafe { external_pid_parts(moved[0]) };
    assert_eq!(c_node.raw(), node.raw());
    assert_eq!((c_num, c_ser, c_creation), (0x1234, 0x0007, 5));
}

#[test]
fn fragment_accounting_reports_geometry() {    let mut src = Heap::new(32).expect("source heap");
    let term = build_list(&mut src, &[make_small(1), make_small(2), make_small(3)]);
    let src_off = OffHeapTable::new();
    let needed = size_of_copy(term);
    assert_eq!(needed, 6, "three cons cells");

    // A fresh fragment is empty but fully available.
    let mut fragment = HeapFragment::new(16).expect("the fragment allocates");
    assert_eq!(fragment.capacity(), 16);
    assert_eq!(fragment.used(), 0);
    assert_eq!(fragment.available(), 16);
    assert!(fragment.roots().is_empty());

    // Copying a root fills exactly the payload's words and records the root.
    let root = fragment.copy_root(term, &src_off).expect("the copy fits");
    assert_eq!(fragment.used(), needed);
    assert_eq!(fragment.available(), 16 - needed);
    assert_eq!(fragment.roots(), &[root]);
    assert!(fragment.contains(root));

    // `Fragments` sums the chain's used words.
    let mut chain = Fragments::new();
    assert!(chain.is_empty());
    assert_eq!(chain.total_used_words(), 0);
    chain.push(fragment);
    assert_eq!(chain.len(), 1);
    assert_eq!(chain.total_used_words(), needed);
    assert_eq!(chain.off_heap_overhead_bytes(), 0);
    assert!(Fragments::default().is_empty());
}

#[test]
fn a_fragment_too_small_for_the_payload_rejects_before_writing() {
    let mut src = Heap::new(64).expect("source heap");
    let term = build_list(&mut src, &[make_small(1), make_small(2), make_small(3)]);
    let needed = size_of_copy(term);
    assert_eq!(needed, 6, "three cons cells");
    let src_off = OffHeapTable::new();

    // A fragment one word too small: `copy_struct`'s capacity preflight rejects before any word is
    // written, so there is NO half-copied root and NO registered off-heap reference.
    let mut fragment = HeapFragment::new(needed - 1).expect("the fragment allocates");
    match fragment.copy_root(term, &src_off) {
        Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Message),
        other => panic!("expected a message allocation failure, got {other:?}"),
    }
    assert_eq!(fragment.used(), 0, "the preflight rejected before any allocation");
    assert!(fragment.roots().is_empty(), "no half-copied root was recorded");
    assert!(fragment.off_heap().is_empty(), "no off-heap reference was registered");
    assert_eq!(fragment.available(), needed - 1, "the whole region is still free");

    // Exactly-sized: the same payload now copies and records one root, filling the region.
    let mut fragment = HeapFragment::new(needed).expect("the fragment allocates");
    let root = fragment.copy_root(term, &src_off).expect("the copy fits exactly");
    assert_eq!(fragment.used(), needed);
    assert_eq!(fragment.available(), 0);
    assert_eq!(fragment.roots(), &[root]);
}

#[test]
fn send_fragmented_uses_one_fragment_sized_to_the_whole_payload() {
    let mut src = Heap::new(256).expect("source heap");
    let elems: Vec<Eterm> = (0..30).map(make_small).collect();
    let term = build_list(&mut src, &elems);
    let needed = size_of_copy(term);
    assert_eq!(needed, 60, "thirty cons cells");
    let src_off = OffHeapTable::new();

    // A `fragment_capacity_words` hint far SMALLER than the payload is ignored: the single fragment
    // is sized to the whole payload (`size_object`), never split across a chain.
    let mut queue = Queue::new();
    {
        let mut to_heap = Heap::new(2).expect("too small for the payload");
        let mut dst_off = OffHeapTable::new();
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        send_fragmented(make_small(0), term, &mut to_heap, &mut queue, &mut ctx, 1)
            .expect("the fragmented send succeeds");
    }
    let message = queue.peek().expect("one queued message");
    let fragments = message.fragments().expect("a fragmented message");
    assert_eq!(fragments.len(), 1, "one fragment, never a chain");
    let only = fragments.iter().next().expect("one fragment");
    assert_eq!(only.capacity(), needed, "sized to the whole payload, not the hint");
    assert_eq!(only.used(), needed, "the whole payload is in the single fragment");
    assert!(compare::eq(term, message.payload(), &AtomTable::new()));

    // A hint LARGER than the payload is honoured (the caller's reservation).
    let mut queue = Queue::new();
    {
        let mut to_heap = Heap::new(2).expect("too small for the payload");
        let mut dst_off = OffHeapTable::new();
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        send_fragmented(make_small(0), term, &mut to_heap, &mut queue, &mut ctx, needed + 8)
            .expect("the fragmented send succeeds");
    }
    let message = queue.peek().expect("one queued message");
    let only = message.fragments().expect("fragmented").iter().next().expect("one fragment");
    assert_eq!(only.capacity(), needed + 8, "a larger hint is honoured");
    assert_eq!(only.used(), needed, "only the payload's words are used");
}

#[test]
fn a_fragmented_message_transfers_both_an_mso_and_a_refc_binary_handle() {
    const PAYLOAD: u32 = 0x00C0_FFEE;
    let bytes: Vec<u8> = (0..256usize).map(|i| ((i * 13) % 251) as u8).collect();

    let mut src_off = OffHeapTable::new();
    assert_eq!(src_off.push(Mso::new(PAYLOAD, 24)), 0);
    src_off.push_binary(RefcBinary::new(&bytes).expect("the payload allocates"));
    let data_ptr = src_off.binaries()[0].data_ptr();

    let mut src = Heap::new(64).expect("source heap");
    let container = bin_ref(&mut src, 0);
    // SAFETY: `src_off` owns the payload, which outlives `term`.
    let binary = unsafe { refc_binary_term(&mut src, data_ptr, bytes.len()) };
    let term = src
        .tuple(&[container, binary, make_small(5)])
        .expect("the tuple fits the source heap");

    let mut to_heap = Heap::new(2).expect("far too small for the payload");
    let mut queue = Queue::new();
    let mut dst_off = OffHeapTable::new();
    {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        send_fragmented(make_small(0), term, &mut to_heap, &mut queue, &mut ctx, 0)
            .expect("the fragmented send succeeds");
    }
    assert_eq!(to_heap.used(), 0, "the payload lives in the fragment");
    assert!(dst_off.is_empty(), "the fragment path does not use the receiver's table");

    // The fragment owns BOTH off-heap objects (an MSO handle and a refc-binary handle).
    let mut message = queue.dequeue().expect("the fragmented message");
    let fragment_root = message.payload();
    let only = message.fragments().expect("fragmented").iter().next().expect("one fragment");
    assert!(only.contains(fragment_root), "the payload points into the fragment");
    assert!(message.off_heap().is_empty() && message.binaries().is_empty());
    assert_eq!(only.off_heap().len(), 1, "the fragment owns the MSO object");
    assert_eq!(only.off_heap().binaries().len(), 1, "the fragment owns the refc binary");
    assert_eq!(src_off.get(0).map(MsoHandle::refcount), Some(2), "source + fragment");
    assert_eq!(src_off.binaries()[0].refcount(), 2, "source + fragment");

    let mut receiver_heap = Heap::new(64).expect("receiver heap");
    let mut receiver_off = OffHeapTable::new();
    let moved = message
        .fragments_mut()
        .expect("fragmented")
        .move_into(&mut receiver_heap, &mut receiver_off)
        .expect("the move succeeds");

    assert_eq!(moved.len(), 1);
    assert!(on_heap(&receiver_heap, moved[0]), "the moved root lives on the receiver heap");
    assert!(
        !message.fragments().expect("fragmented").iter().next().unwrap().contains(moved[0]),
        "the payload moved off the fragment"
    );
    // Both off-heap objects transferred to the receiver's table, as HANDLES.
    assert_eq!(receiver_off.get(0).and_then(|h| h.downcast_ref::<u32>()), Some(&PAYLOAD));
    assert_eq!(receiver_off.binaries().len(), 1);
    assert_eq!(receiver_off.binaries()[0].data_ptr(), data_ptr, "the payload is shared");
    assert_eq!(receiver_off.binaries()[0].as_bytes(), &bytes[..]);
    assert_eq!(receiver_off.binaries()[0].refcount(), 2, "source + receiver");
    assert_eq!(src_off.get(0).map(MsoHandle::refcount), Some(2), "source + receiver");
    assert_eq!(src_off.binaries()[0].refcount(), 2, "source + receiver");
    // The fragment gave up both references.
    let given_up = message.fragments().expect("fragmented").iter().next().unwrap();
    assert!(given_up.off_heap().is_empty(), "the fragment gave up its MSO reference");
    assert!(
        given_up.off_heap().binaries().is_empty(),
        "the fragment gave up its refc-binary reference"
    );
}

#[test]
fn a_fragmented_hash_map_moves_off_the_fragment_and_stays_equal() {
    let atoms = AtomTable::new();
    // 40 distinct small-integer keys exceed `MAP_SMALL_MAP_LIMIT` (32) => a genuine HAMT (hash map).
    let pairs: Vec<(Eterm, Eterm)> = (0..40)
        .map(|i| (make_small(i), make_small(i * 1000 + 7)))
        .collect();

    let mut src = Heap::new(map_words_capacity(40) + 16).expect("source heap");
    // SAFETY: `alloc_words` returns `map_words_capacity(40)` exclusively-owned words, which
    // `write_map` fills entirely.
    let map = unsafe {
        let hp = src.alloc_words(map_words_capacity(40)).expect("the map fits");
        write_map(hp, &pairs, &atoms).expect("the 40-key map builds")
    };
    // SAFETY: `map` is a live boxed map term.
    assert!(!is_flatmap_header(unsafe { *boxed_val(map) }), "40 > 32 keys => a hash map");

    let src_off = OffHeapTable::new();
    let mut to_heap = Heap::new(4).expect("far too small for the trie");
    let mut queue = Queue::new();
    {
        let mut dst_off = OffHeapTable::new();
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        send_fragmented(make_small(0), map, &mut to_heap, &mut queue, &mut ctx, 0)
            .expect("the fragmented send succeeds");
    }
    assert_eq!(to_heap.used(), 0);

    let mut message = queue.dequeue().expect("the fragmented message");
    let only = message.fragments().expect("fragmented").iter().next().expect("one fragment");
    assert!(only.contains(message.payload()), "the whole trie is in the fragment");
    assert_eq!(only.used(), size_of_copy(map), "the whole trie is in the fragment");

    let mut receiver_heap = Heap::new(size_of_copy(map) + 16).expect("receiver heap");
    let mut receiver_off = OffHeapTable::new();
    let moved = message
        .fragments_mut()
        .expect("fragmented")
        .move_into(&mut receiver_heap, &mut receiver_off)
        .expect("the move succeeds");

    assert_eq!(moved.len(), 1);
    assert!(on_heap(&receiver_heap, moved[0]), "the trie moved onto the receiver heap");
    assert!(compare::eq(map, moved[0], &atoms), "the moved trie must be =:= the source");
    assert_eq!(map_size(moved[0], &atoms).expect("a well-formed map"), 40);
    for (k, v) in &pairs {
        assert_eq!(
            map_get(moved[0], *k, &atoms).expect("a well-formed map"),
            Some(*v),
            "key {k:?} must resolve to {v:?} on the moved trie"
        );
    }
    assert!(receiver_off.is_empty(), "a map references no off-heap objects");
    assert!(
        !message.fragments().expect("fragmented").iter().next().unwrap().contains(moved[0]),
        "the payload moved off the fragment"
    );
}

#[test]
fn a_fragmented_closure_moves_off_the_fragment_and_stays_equal() {
    const UNIQ: u64 = 0x0102_0304_0506_0708;
    const INDEX: u32 = 0x0A0B_0C0D;
    let atoms = AtomTable::new();

    let mut src = Heap::new(64).expect("source heap");
    let boxed_env = src
        .tuple(&[make_small(11), make_small(22)])
        .expect("the env tuple fits the source heap");
    // A `make_fun3` closure with a *boxed* free variable: `[header][uniq][index][env[0..]]`.
    // SAFETY: `fun_words(Local, 2)` is the block's exact size and `write_fun_local` writes exactly
    // those words; every `env` term is live.
    let fun = unsafe {
        let hp = src
            .alloc_words(fun_words(FunKind::Local, 2))
            .expect("the fun block fits the source heap");
        write_fun_local(hp, 1, UNIQ, INDEX, &[boxed_env, make_small(3)])
            .expect("a valid local-fun header")
    };

    let src_off = OffHeapTable::new();
    let mut to_heap = Heap::new(2).expect("far too small for the closure");
    let mut queue = Queue::new();
    {
        let mut dst_off = OffHeapTable::new();
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        send_fragmented(make_small(0), fun, &mut to_heap, &mut queue, &mut ctx, 0)
            .expect("the fragmented send succeeds");
    }
    assert_eq!(to_heap.used(), 0);

    let mut message = queue.dequeue().expect("the fragmented message");
    let only = message.fragments().expect("fragmented").iter().next().expect("one fragment");
    assert!(only.contains(message.payload()), "the closure is in the fragment");

    let mut receiver_heap = Heap::new(size_of_copy(fun) + 8).expect("receiver heap");
    let mut receiver_off = OffHeapTable::new();
    let moved = message
        .fragments_mut()
        .expect("fragmented")
        .move_into(&mut receiver_heap, &mut receiver_off)
        .expect("the move succeeds");

    assert_eq!(moved.len(), 1);
    assert!(on_heap(&receiver_heap, moved[0]), "the closure moved onto the receiver heap");
    assert!(compare::eq(fun, moved[0], &atoms), "the moved closure must be =:= the source");
    // The whole environment came along: the boxed free variable is fixed up into the receiver heap
    // (the copier's `env[0]` offset is `1 + desc_words(kind)`, not the legacy `ERL_FUN_SIZE`).
    // SAFETY: `moved[0]` is a live local fun.
    let env = unsafe { fun_env(moved[0]).expect("a local fun carries its environment") };
    assert_eq!(env.len(), 2);
    assert!(
        on_heap(&receiver_heap, env[0]) || is_literal(env[0]),
        "the boxed free variable lives on the receiver heap"
    );
    assert_eq!(signed_value(env[1]), 3);
    assert!(
        !message.fragments().expect("fragmented").iter().next().unwrap().contains(moved[0]),
        "the payload moved off the fragment"
    );
}

#[test]
fn a_multi_fragment_chain_moves_in_order_with_consistent_accounting() {
    const PAYLOAD: u32 = 0x0000_ABCD;
    let mut src_off = OffHeapTable::new();
    assert_eq!(src_off.push(Mso::new(PAYLOAD, 40)), 0);

    let mut src = Heap::new(64).expect("source heap");
    // Fragment A's root references the MSO object through a container (the fragment's off-heap
    // table must own it); fragment B's root is a plain list.
    let container = bin_ref(&mut src, 0);
    let with_mso = src
        .tuple(&[container, make_small(1)])
        .expect("the tuple fits the source heap");
    let plain = build_list(&mut src, &[make_small(2), make_small(3)]);

    let mut frag_a = HeapFragment::new(size_of_copy(with_mso)).expect("fragment A");
    frag_a.copy_root(with_mso, &src_off).expect("root A fits");
    let mut frag_b = HeapFragment::new(size_of_copy(plain)).expect("fragment B");
    frag_b.copy_root(plain, &src_off).expect("root B fits");
    assert_eq!(frag_a.off_heap().len(), 1, "fragment A owns the MSO object");
    assert_eq!(src_off.get(0).map(MsoHandle::refcount), Some(2), "source + fragment A");

    let mut chain = Fragments::new();
    chain.push(frag_a);
    chain.push(frag_b);

    // Accounting is consistent across the chain: `used`/`capacity`/`available` per fragment, and
    // the chain's `total_used_words`/`off_heap_overhead_bytes` are the per-fragment sums.
    assert_eq!(chain.len(), 2);
    assert_eq!(
        chain.total_used_words(),
        size_of_copy(with_mso) + size_of_copy(plain),
        "the summed `used_size`"
    );
    assert_eq!(
        chain.off_heap_overhead_bytes(),
        chain.iter().map(HeapFragment::off_heap_overhead_bytes).sum::<usize>()
    );
    assert_eq!(chain.off_heap_overhead_bytes(), 40, "the one MSO's apparent size");
    for fragment in chain.iter() {
        assert_eq!(fragment.available(), fragment.capacity() - fragment.used());
    }
    assert_eq!(
        chain.iter().map(HeapFragment::used).collect::<Vec<_>>(),
        vec![size_of_copy(with_mso), size_of_copy(plain)],
        "each fragment reports its own `used_size`, in chain order"
    );

    let mut dst = Heap::new(64).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let moved = chain.move_into(&mut dst, &mut dst_off).expect("the move succeeds");

    assert_eq!(moved.len(), 2, "both roots are moved back");
    assert!(on_heap(&dst, moved[0]) && on_heap(&dst, moved[1]), "both live on the destination heap");
    // Chain order: fragment A's tuple first, fragment B's list second.
    assert!(!is_list(moved[0]), "the first root is fragment A's tuple");
    assert!(is_list(moved[1]), "the second root is fragment B's list");
    // SAFETY: `moved[0]` is a live 2-tuple.
    assert_eq!(unsafe { signed_value(tuple_element(moved[0], 1)) }, 1);
    assert!(compare::eq(plain, moved[1], &AtomTable::new()));

    assert_eq!(dst_off.get(0).and_then(|h| h.downcast_ref::<u32>()), Some(&PAYLOAD));
    assert_eq!(src_off.get(0).map(MsoHandle::refcount), Some(2), "source + destination");
    assert!(
        chain.iter().all(|f| f.off_heap().is_empty()),
        "every fragment gave up its references after the whole chain moved"
    );
}

#[test]
fn a_failed_move_into_leaves_the_chain_intact() {
    const PAYLOAD: u32 = 0x0000_1234;
    let mut src_off = OffHeapTable::new();
    assert_eq!(src_off.push(Mso::new(PAYLOAD, 16)), 0);

    let mut src = Heap::new(64).expect("source heap");
    // Fragment A's root owns the MSO object (through a container); fragment B is a longer list.
    let with_mso = bin_ref(&mut src, 0);
    let big_list = build_list(
        &mut src,
        &[make_small(1), make_small(2), make_small(3), make_small(4), make_small(5)],
    );

    let mut frag_a = HeapFragment::new(size_of_copy(with_mso)).expect("fragment A");
    frag_a.copy_root(with_mso, &src_off).expect("root A fits");
    let mut frag_b = HeapFragment::new(size_of_copy(big_list)).expect("fragment B");
    frag_b.copy_root(big_list, &src_off).expect("root B fits");
    assert!(size_of_copy(big_list) > size_of_copy(with_mso));

    let mut chain = Fragments::new();
    chain.push(frag_a);
    chain.push(frag_b);

    // A destination heap that fits fragment A's root but NOT fragment B's: the move fails on the
    // second root, after the first was already copied.
    let mut dst = Heap::new(size_of_copy(with_mso)).expect("fits only root A");
    let mut dst_off = OffHeapTable::new();
    match chain.move_into(&mut dst, &mut dst_off) {
        Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Message),
        other => panic!("expected a message allocation failure, got {other:?}"),
    }

    // The partial copy did register the object (it is the caller's to discard) ...
    assert_eq!(dst_off.len(), 1, "root A's copy registered the off-heap object");
    // ... but NO fragment gave up its references: the chain is left intact (no half-stripped chain).
    let a = chain.iter().next().expect("fragment A");
    assert_eq!(a.off_heap().len(), 1, "fragment A still owns its reference");
    let b = chain.iter().nth(1).expect("fragment B");
    assert!(b.off_heap().is_empty(), "fragment B referenced no off-heap object");
    assert_eq!(
        src_off.get(0).map(MsoHandle::refcount),
        Some(3),
        "source + fragment A + the partial destination copy"
    );
    assert_eq!(dst.used(), size_of_copy(with_mso), "root A's words were written");
}
