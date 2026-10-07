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
//!    payload never deep-copied, refcount incremented),
//! 5. fragment accounting (`used`/`capacity`/`total_used_words`; the receiver heap stays empty).

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

    // 1. Plain `send` fails with the typed message allocation failure and touches nothing.
    {
        let mut dst_off = OffHeapTable::new();
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        match send(from, term, &mut to_heap, &mut queue, &mut ctx) {
            Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Message),
            other => panic!("expected a message allocation failure, got {other:?}"),
        }
    }
    assert_eq!(to_heap.used(), 0, "the failed copy wrote nothing");
    assert!(queue.is_empty());

    // 2. `send_fragmented` turns the same send into a fragment-backed message.
    {
        let mut dst_off = OffHeapTable::new();
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        send_fragmented(from, term, &mut to_heap, &mut queue, &mut ctx, size_of_copy(term))
            .expect("the fragmented send succeeds");
    }
    assert_eq!(to_heap.used(), 0, "the payload lives in the fragment, not the receiver heap");

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
fn fragment_accounting_reports_geometry() {
    let mut src = Heap::new(32).expect("source heap");
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
