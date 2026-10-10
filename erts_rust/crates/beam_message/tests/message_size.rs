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

//! Integration tests for the **logical-size accounting surface** on
//! [`Message`](beam_message::Message): [`heap_words`](beam_message::Message::heap_words) (the
//! sender-charge / `size_object_litopt` analogue), [`logical_binary_bytes`](beam_message::Message::logical_binary_bytes)
//! (the holder's virtual-binary-heap input) and the relocation-safe envelope rebuild
//! [`with_payload`](beam_message::Message::with_payload).
//!
//! The measured semantics:
//!
//! * `heap_words()` counts **heap words** — the payload graph's flat word count for a heap-resident
//!   message, the fragment chain's used words for a fragmented one — so an off-heap refc binary
//!   contributes only its 8-word on-heap handle,
//! * `logical_binary_bytes()` counts the *logical* payload bytes of the refc binaries the message
//!   shares (a disjoint figure, accounted to the holder),
//! * `with_payload` carries `from`/`token`/`off_heap`/`binaries`/`nodes`/`fragments` across the
//!   rebuild, so a collector relocating a queued message never drops them.

use beam_alloc::{Heap, RefcBinary};
use beam_gc::Mso;
use beam_message::off_heap::OffHeapTable;
use beam_message::{
    send_fragmented, size_of_copy, CopyContext, Fragments, HeapFragment, Message, NodeEntry, Queue,
};
use beam_term::prelude::*;

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first, synchronously (no recursion).
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the list fits the source heap");
    }
    acc
}

/// A 200-byte payload whose bytes are position-dependent, so a partial or shifted copy is visible.
fn bytes_200() -> Vec<u8> {
    (0..200usize).map(|i| ((i * 7) % 251) as u8).collect()
}

/// Writes the on-heap refc-binary pair for `data_ptr`/`len` and returns the tagged root (the
/// `ErlSubBits`).
///
/// # Safety
///
/// `heap` must have 8 spare words and `data_ptr` must be a live, 4-byte-aligned payload of `len`
/// bytes that outlives the returned term.
unsafe fn refc_binary_term(heap: &mut Heap, data_ptr: *mut u8, len: usize) -> Eterm {
    // SAFETY: the caller keeps the payload alive; `data_ptr` is `UNIT_SIZE`-aligned (`data_ptr` is
    // `base + HEADER_SIZE` with `HEADER_SIZE == UNIT_SIZE`), satisfying the flag-bit assert.
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
fn a_heap_resident_payload_reports_its_flat_word_count() {
    let mut heap = Heap::new(64).expect("the heap allocates");
    let payload = build_list(&mut heap, &[make_small(1), make_small(2), make_small(3)]);
    assert_eq!(beam_term::size_object(payload), 6, "three cons cells");

    let message = Message::new(payload, make_small(7));
    assert!(message.fragments().is_none(), "the payload lives in the receiver heap");
    assert_eq!(message.heap_words(), beam_term::size_object(payload));
    assert_eq!(message.heap_words(), 6);

    // An immediate is not a heap object, so it occupies no heap words at all.
    let immediate = Message::new(make_small(3), make_small(7));
    assert_eq!(immediate.heap_words(), beam_term::size_object(make_small(3)));
    assert_eq!(immediate.heap_words(), 0);
}

#[test]
fn a_fragmented_payload_reports_the_fragment_chains_used_words() {
    let mut src = Heap::new(512).expect("source heap");
    let elems: Vec<Eterm> = (0..100).map(make_small).collect();
    let term = build_list(&mut src, &elems);
    let copied_words = size_of_copy(term);
    assert_eq!(copied_words, 200, "100 cons cells");

    // A receiver heap far too small for the payload: the send must fragment.
    let mut to_heap = Heap::new(4).expect("a deliberately too-small receiver heap");
    let mut queue = Queue::new();
    let src_off = OffHeapTable::new();
    let mut dst_off = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src_off, &mut dst_off);

    send_fragmented(
        make_small(7),
        term,
        &mut to_heap,
        &mut queue,
        &mut ctx,
        copied_words,
    )
    .expect("the oversized send fragments instead of failing");

    assert_eq!(to_heap.used(), 0, "a fragmented payload does not touch the receiver heap");
    let message = queue.peek().expect("one queued message");
    let fragments = message.fragments().expect("the payload lives in the fragments");
    assert_eq!(fragments.len(), 1, "one fragment holds the whole payload");

    assert_eq!(message.heap_words(), fragments.total_used_words());
    assert_eq!(
        message.heap_words(),
        copied_words,
        "the fragment's used words are the payload's flat word count"
    );
}

#[test]
fn a_fragmented_message_built_by_copy_root_reports_the_same() {
    let mut src = Heap::new(64).expect("source heap");
    let term = build_list(&mut src, &[make_small(1), make_small(2), make_small(3)]);

    let src_off = OffHeapTable::new();
    let mut fragment = HeapFragment::new(16).expect("the fragment allocates");
    fragment.copy_root(term, &src_off).expect("the copy fits");
    assert_eq!(fragment.used(), 6);

    let mut fragments = Fragments::new();
    fragments.push(fragment);
    let message = Message::new(term, make_small(0)).with_fragments(fragments);

    assert_eq!(message.heap_words(), 6);
    assert_eq!(
        message.heap_words(),
        message.fragments().expect("fragments").total_used_words()
    );
}

#[test]
fn logical_binary_bytes_counts_the_shared_refc_payloads() {
    let bytes = bytes_200();
    let source_binary = RefcBinary::new(&bytes).expect("the payload allocates");
    let data_ptr = source_binary.data_ptr();

    // The message's payload is the on-heap 8-word refc-binary handle; the 200-byte payload is
    // off-heap and shared.
    let mut heap = Heap::new(32).expect("the heap allocates");
    // SAFETY: `source_binary` outlives the returned term (it is moved into the message below) and
    // `data_ptr` is `UNIT_SIZE`-aligned.
    let payload = unsafe { refc_binary_term(&mut heap, data_ptr, bytes.len()) };
    assert!(unsafe { is_refc_binary(payload) });

    let message = Message::new(payload, make_small(7)).with_binaries(vec![source_binary]);
    assert_eq!(message.binaries().len(), 1);

    // The LOGICAL size is the payload byte count; the HEAP size is only the 8-word handle.
    assert_eq!(message.logical_binary_bytes(), 200);
    assert_eq!(message.heap_words(), ERL_REFC_BITS_SIZE);
    assert_eq!(message.heap_words(), 8);
    assert!(
        message.heap_words() < message.logical_binary_bytes(),
        "the heap charge ignores the payload's bulk"
    );

    // A second message sharing the same logical size, and a message with no binaries at all.
    assert_eq!(
        Message::new(make_small(1), make_small(7))
            .with_binaries(vec![RefcBinary::new(&bytes).expect("the payload allocates")])
            .logical_binary_bytes(),
        200
    );
    assert_eq!(
        Message::new(make_small(1), make_small(7)).logical_binary_bytes(),
        0,
        "no shared refc binaries ⇒ no logical binary bytes"
    );

    // A big **heap** payload (no binaries) reports 0 logical bytes — the two figures are disjoint.
    let mut big = Heap::new(64).expect("the heap allocates");
    let list = build_list(&mut big, &[make_small(1), make_small(2), make_small(3)]);
    let plain = Message::new(list, make_small(7));
    assert_eq!(plain.logical_binary_bytes(), 0);
    assert_eq!(plain.heap_words(), 6);
}

#[test]
fn with_payload_preserves_the_whole_envelope() {
    let mut atoms = AtomTable::new();
    let node = make_atom(atoms.intern("nonode@nohost"));

    let bytes = bytes_200();
    let binary = RefcBinary::new(&bytes).expect("the payload allocates");

    // One MSO handle, taken out of its table (the builder takes an owned `Vec<MsoHandle>`).
    let mut off = OffHeapTable::new();
    off.push(Mso::new(0xABCD_u32, 8));
    let handles = off.into_handles();
    assert_eq!(handles.len(), 1);

    let mut fragments = Fragments::new();
    fragments.push(HeapFragment::new(16).expect("the fragment allocates"));

    let from = make_small(7);
    let token = make_small(9);
    let original_payload = make_small(1);
    let message = Message::new(original_payload, from)
        .with_token(token)
        .with_off_heap(handles)
        .with_binaries(vec![binary])
        .with_nodes(vec![NodeEntry::new(node, 2)])
        .with_fragments(fragments);

    let replacement = make_small(42);
    let rebuilt = message.with_payload(replacement);

    assert_eq!(rebuilt.payload(), replacement, "the new payload is in place");
    assert_eq!(rebuilt.from(), from, "the sender survives the rebuild");
    assert_eq!(rebuilt.token(), token, "the token survives the rebuild");

    assert_eq!(rebuilt.off_heap().len(), 1, "the off-heap handles survive the rebuild");
    assert_eq!(
        rebuilt.off_heap()[0].downcast_ref::<u32>(),
        Some(&0xABCD_u32)
    );

    assert_eq!(rebuilt.binaries().len(), 1, "the refc binaries survive the rebuild");
    assert_eq!(rebuilt.binaries()[0].as_bytes(), &bytes[..]);
    assert_eq!(rebuilt.logical_binary_bytes(), 200);

    assert_eq!(rebuilt.nodes().len(), 1, "the node references survive the rebuild");
    assert_eq!(rebuilt.nodes()[0].node().raw(), node.raw());
    assert_eq!(rebuilt.nodes()[0].creation(), 2);

    let fragments = rebuilt.fragments().expect("the fragment chain survives the rebuild");
    assert_eq!(fragments.len(), 1);
}
