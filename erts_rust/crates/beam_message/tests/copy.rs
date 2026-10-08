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

//! Integration tests for the copy engine ([`copy_struct`](beam_message::copy_struct) /
//! [`copy_struct_simple`](beam_message::copy_struct_simple)).
//!
//! Covers the required evidence of the crate `CONTEXT.md` `Test Strategy`:
//!
//! 1. nested-graph structural equality (a real copy on the destination heap),
//! 2. a deep list copied without native-stack recursion,
//! 3. literal sharing (returned by identity, the destination heap untouched),
//! 4. MSO refcount preservation (refcounted, never deep-copied) and the slot rewrite,
//! 5. heap exhaustion reported as `BeamError::Alloc(AllocKind::Message)` and never a panic.

use beam_alloc::Heap;
use beam_common::{AllocKind, BeamError};
use beam_gc::Mso;
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, copy_struct_simple, size_of_copy, CopyContext};
use beam_term::prelude::*;
use beam_term::{compare, size_object};

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first, synchronously (no recursion).
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the list fits the source heap");
    }
    acc
}

/// Walks `term` as a cons list and returns the number of cells, iteratively.
fn count_cells(term: Eterm) -> usize {
    let mut count = 0;
    let mut current = term;
    while is_list(current) {
        count += 1;
        // SAFETY: `current` is a live, non-`NIL` cons cell.
        current = unsafe { cdr(current) };
    }
    count
}

/// Builds an off-heap reference container `[HEADER_BIN_REF, slot, 0]` on `heap`.
fn bin_ref(heap: &mut Heap, slot: usize) -> Eterm {
    // SAFETY: the heap owns these three words.
    unsafe {
        let p = heap.alloc_words(3).expect("the bin-ref fits");
        *p = HEADER_BIN_REF;
        *p.add(1) = Eterm::from_raw(slot);
        *p.add(2) = Eterm::from_raw(0);
        make_boxed(p as *const Eterm)
    }
}

#[test]
fn nested_graph_is_copied_structurally_equal_onto_the_destination_heap() {
    let mut atoms = AtomTable::new();
    let answer = make_atom(atoms.intern("answer"));

    let mut src = Heap::new(256).expect("source heap");
    let numbers = build_list(&mut src, &[make_small(1), make_small(2), answer]);
    let pair = src.tuple(&[make_small(7), make_small(8)]).expect("the pair fits");
    let nested = src
        .tuple(&[numbers, pair, make_small(42), answer])
        .expect("the nested tuple fits");
    let original = build_list(&mut src, &[nested, make_small(0)]);

    let mut dst = Heap::new(256).expect("destination heap");
    let copy = copy_struct_simple(original, &mut dst).expect("the copy succeeds");

    assert!(compare::eq(original, copy, &atoms), "the copy must be =:= the original");
    assert_eq!(dst.used(), size_of_copy(original), "the copy uses the sized word count");
    assert_eq!(dst.used(), size_object(original));
    // Different addresses prove a genuine copy rather than a shared pointer.
    assert_ne!(list_val(original) as usize, list_val(copy) as usize);
}

#[test]
fn deep_list_copies_without_native_stack_recursion() {
    const N: usize = 100_000;
    let mut src = Heap::new(2 * N + 16).expect("source heap");
    let mut list = NIL;
    for i in 0..N {
        list = src
            .cons(make_small((i % 1000) as i64), list)
            .expect("each cell fits");
    }

    let mut dst = Heap::new(2 * N + 16).expect("destination heap");
    let copy = copy_struct_simple(list, &mut dst).expect("the deep copy succeeds");

    assert_eq!(count_cells(copy), N, "every cell must have been copied");
    let atoms = AtomTable::new();
    assert!(compare::eq(list, copy, &atoms));
    assert_ne!(list_val(list) as usize, list_val(copy) as usize);
}

#[test]
fn literal_compound_is_shared_not_copied() {
    let mut src = Heap::new(16).expect("source heap");
    let tuple = src.tuple(&[make_small(1), make_small(2)]).expect("the tuple fits");
    let literal = make_literal(tuple);

    let mut dst = Heap::new(16).expect("destination heap");
    let copy = copy_struct_simple(literal, &mut dst).expect("the copy succeeds");

    assert_eq!(copy.raw(), literal.raw(), "a literal is returned by identity");
    assert_eq!(dst.used(), 0, "a shared literal touches no destination word");
    assert!(is_literal(copy));
}

#[test]
fn off_heap_reference_is_refcounted_not_deep_copied() {
    const PAYLOAD: u32 = 0xDEAD_BEEF;

    let mut src_off_heap = OffHeapTable::new();
    let slot = src_off_heap.push(Mso::new(PAYLOAD, 8));
    assert_eq!(slot, 0);
    assert_eq!(src_off_heap.get(slot).map(|h| h.refcount()), Some(1));

    let mut src = Heap::new(16).expect("source heap");
    let container = bin_ref(&mut src, slot);
    let outer = src.tuple(&[make_small(1), container]).expect("the outer tuple fits");

    let mut dst = Heap::new(16).expect("destination heap");
    let mut dst_off_heap = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);
        copy_struct(outer, &mut dst, &mut ctx).expect("the copy succeeds")
    };

    // Exactly one handle in the destination, refcount 2 (source + destination), same object.
    assert_eq!(dst_off_heap.len(), 1);
    let dst_handle = dst_off_heap.get(0).expect("destination slot 0");
    assert_eq!(dst_handle.refcount(), 2);
    assert_eq!(dst_handle.downcast_ref::<u32>(), Some(&PAYLOAD));
    assert_eq!(src_off_heap.get(slot).map(|h| h.refcount()), Some(2));

    // The copied container points at a valid destination slot resolving to the same object.
    let copied_container = unsafe { tuple_element(copy, 1) };
    let copied_slot = unsafe { *boxed_val(copied_container).add(1) }.raw();
    assert_eq!(copied_slot, 0);
    assert_eq!(
        dst_off_heap
            .get(copied_slot)
            .and_then(|h| h.downcast_ref::<u32>()),
        Some(&PAYLOAD),
        "the copied container resolves to the same (not deep-copied) off-heap object"
    );

    // Dropping the destination table releases only the destination's reference.
    drop(dst_off_heap);
    assert_eq!(src_off_heap.get(slot).map(|h| h.refcount()), Some(1));
}

#[test]
fn off_heap_slot_is_rewritten_to_the_destination_slot() {
    let mut src_off_heap = OffHeapTable::new();
    let _decoy = src_off_heap.push(Mso::new(1u32, 4));
    let slot = src_off_heap.push(Mso::new(2u32, 4));
    assert_eq!(slot, 1, "the referenced object lives at a non-zero source slot");

    let mut src = Heap::new(8).expect("source heap");
    let container = bin_ref(&mut src, slot);

    let mut dst = Heap::new(8).expect("destination heap");
    let mut dst_off_heap = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);
        copy_struct(container, &mut dst, &mut ctx).expect("the copy succeeds")
    };

    // The source slot (1) is remapped to the fresh destination slot (0).
    let copied_slot = unsafe { *boxed_val(copy).add(1) }.raw();
    assert_eq!(copied_slot, 0);
    assert_eq!(dst_off_heap.len(), 1);
    assert_eq!(
        dst_off_heap
            .get(copied_slot)
            .and_then(|h| h.downcast_ref::<u32>()),
        Some(&2)
    );
}

#[test]
fn heap_exhaustion_is_a_message_allocation_failure_not_a_panic() {
    let mut src = Heap::new(128).expect("source heap");
    let list = build_list(&mut src, &[make_small(1); 50]);
    assert_eq!(size_of_copy(list), 100, "50 cons cells are 100 words");

    let mut dst = Heap::new(4).expect("a destination heap far too small");
    match copy_struct_simple(list, &mut dst) {
        Err(BeamError::Alloc(failure)) => {
            assert_eq!(failure.kind, AllocKind::Message);
            assert_eq!(failure.requested, 100 * core::mem::size_of::<Eterm>());
        }
        other => panic!("expected a message allocation failure, got {other:?}"),
    }
    assert_eq!(dst.used(), 0, "the preflight rejects before any allocation");
}

#[test]
fn out_of_range_off_heap_slot_is_a_bad_term() {
    // A container whose slot has no entry in the source table is a malformed graph.
    let src_off_heap = OffHeapTable::new();
    let mut src = Heap::new(8).expect("source heap");
    let container = bin_ref(&mut src, 7);

    let mut dst = Heap::new(8).expect("destination heap");
    let mut dst_off_heap = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);
    match copy_struct(container, &mut dst, &mut ctx) {
        Err(BeamError::BadArg) => {}
        other => panic!("expected BadArg for an out-of-range off-heap slot, got {other:?}"),
    }
}

/// Counts the inner (`MAP_HEADER_TAG_HAMT_NODE_BITMAP`) nodes reachable from the hash-map node
/// `node`, iteratively, so a 40-key trie's subtrees are all inspected. Leaf cons cells are `LIST`
/// words and collision tuples are `ARITYVAL` boxed terms (which hold only cells), so both are
/// skipped — they can contain no further sub-node.
fn count_inner_nodes(node: Eterm) -> usize {
    let mut count = 0;
    let mut stack = vec![node];
    while let Some(term) = stack.pop() {
        if !is_boxed(term) {
            continue;
        }
        // SAFETY: `term` is boxed, so its header word is a live term word.
        let head = unsafe { *boxed_val(term) };
        if !is_map_header(head) {
            continue;
        }
        let arity = header_arity(head);
        let nslots = map_header_val(head).count_ones() as usize;
        if map_header_type(head) as usize == MAP_HEADER_TAG_HAMT_NODE_BITMAP {
            count += 1;
        }
        for i in 0..nslots {
            // SAFETY: slot `i` of the node is within its payload.
            let slot = unsafe { *boxed_val(term).add(1 + arity + i) };
            stack.push(slot);
        }
    }
    count
}

#[test]
fn hash_map_over_32_keys_is_copied_as_a_whole_trie() {
    let atoms = AtomTable::new();

    // 40 distinct small-integer keys exceed `MAP_SMALL_MAP_LIMIT` (32), so `write_map` builds a
    // genuine HAMT (hash map) rather than a flat map.
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
    let src_head = unsafe { *boxed_val(map) };
    assert!(is_map_header(src_head));
    assert!(!is_flatmap_header(src_head), "40 > 32 keys => a hash map");

    let mut dst = Heap::new(size_object(map) + 16).expect("destination heap");
    let copy = copy_struct_simple(map, &mut dst).expect("the copy succeeds");

    // Structural equality: the whole copied trie is `=:=` the original.
    assert!(compare::eq(map, copy, &atoms), "the copy must be =:= the original");
    // A genuine full copy: every trie node, collision tuple and leaf is copied.
    assert_eq!(dst.used(), size_object(map), "the whole trie is copied");

    // Well-formedness: same size, and every key resolves to its original value.
    assert_eq!(map_size(copy, &atoms).expect("a well-formed map"), 40);
    for (k, v) in &pairs {
        assert_eq!(
            map_get(copy, *k, &atoms).expect("a well-formed map"),
            Some(*v),
            "key {k:?} must resolve to {v:?}"
        );
    }

    // The copy is a hash map with the same head kind as the source, and the trie really has
    // sub-nodes (40 keys cannot fit in a single HAMT level).
    // SAFETY: `copy` is a live boxed map term.
    let copy_head = unsafe { *boxed_val(copy) };
    assert!(!is_flatmap_header(copy_head), "still a hash map, not flattened");
    assert_eq!(
        map_header_type(copy_head),
        map_header_type(src_head),
        "the head kind is preserved"
    );
    assert!(
        count_inner_nodes(copy) > 0,
        "the copied trie must contain at least one sub-node"
    );
    assert_eq!(
        count_inner_nodes(copy),
        count_inner_nodes(map),
        "every source sub-node is reproduced"
    );

    // A genuine *collision* node (two distinct keys with an identical 64-bit `term_hash`) is not
    // reachable with distinct small-integer keys, so collision-node coverage is intentionally not
    // forced here — a collision tuple is an ordinary `ARITYVAL` boxed term the copier already
    // walks (see `layout::for_each_child_slot`'s `ARITYVAL_SUBTAG` arm).
}
