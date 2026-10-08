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

//! Integration tests for the **sharing-preserving** copier
//! ([`copy_struct_shared`](beam_message::copy_struct_shared)).
//!
//! Covers the required evidence of the crate `CONTEXT.md` `Test Strategy`:
//!
//! 1. a large subterm repeated `N` times is materialised **once**: the shared copy's destination
//!    word count is ~`1/N` of the flat copy's and every occurrence shares one destination address,
//! 2. the shared copy is structurally `=:=` the original,
//! 3. a graph with **no** sharing copies to exactly the flat size,
//! 4. a shared off-heap MSO container is refcounted **once** per source identity.
//!
//! Note on sizing: `copy_struct_shared` preflights `beam_term::size_object` — the *flat* word count,
//! a safe upper bound — so a destination heap is sized exactly as for `copy_struct`; the saving is
//! in the words the copy actually *writes* (`Heap::used`).

use beam_alloc::Heap;
use beam_gc::Mso;
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, copy_struct_shared, size_of_copy, CopyContext};
use beam_term::prelude::*;
use beam_term::compare;

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first (no recursion).
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the list fits the source heap");
    }
    acc
}

/// The raw word of every cons cell's car, walked iteratively.
fn car_words(term: Eterm) -> Vec<usize> {
    let mut out = Vec::new();
    let mut current = term;
    while is_list(current) {
        // SAFETY: `current` is a live, non-`NIL` cons cell.
        out.push(unsafe { car(current) }.raw());
        // SAFETY: as above.
        current = unsafe { cdr(current) };
    }
    out
}

/// Builds an off-heap reference container `[HEADER_BIN_REF, slot, 0]` on `heap`.
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

fn copy_flat(term: Eterm, src: &OffHeapTable, dst: &mut Heap, dst_off: &mut OffHeapTable) -> Eterm {
    let mut ctx = CopyContext::new(src, dst_off);
    copy_struct(term, dst, &mut ctx).expect("the flat copy succeeds")
}

fn copy_shared(term: Eterm, src: &OffHeapTable, dst: &mut Heap, dst_off: &mut OffHeapTable) -> Eterm {
    let mut ctx = CopyContext::new(src, dst_off);
    copy_struct_shared(term, dst, &mut ctx).expect("the shared copy succeeds")
}

#[test]
fn a_repeated_large_subterm_is_materialised_once() {
    const N: usize = 8;
    const K: usize = 1000;

    let mut src = Heap::new(2 * N + 2 * K + 16).expect("source heap");
    let elems: Vec<Eterm> = (0..K).map(|i| make_small((i % 97) as i64)).collect();
    let sub = build_list(&mut src, &elems);
    let outer = build_list(&mut src, &vec![sub; N]);

    let sub_words = size_of_copy(sub);
    assert_eq!(sub_words, 2 * K, "a {K}-cell list is {K} two-word cons cells");
    let atoms = AtomTable::new();
    let src_off = OffHeapTable::new();

    // Both destinations are sized for the flat copy (the conservative preflight).
    let cap = size_of_copy(outer);
    let mut flat_dst = Heap::new(cap).expect("flat destination heap");
    let mut flat_off = OffHeapTable::new();
    let flat = copy_flat(outer, &src_off, &mut flat_dst, &mut flat_off);

    let mut shared_dst = Heap::new(cap).expect("shared destination heap");
    let mut shared_off = OffHeapTable::new();
    let shared = copy_shared(outer, &src_off, &mut shared_dst, &mut shared_off);

    // (1) The flat copy duplicates the subterm N times; the shared copy stores it once.
    assert_eq!(flat_dst.used(), 2 * N + N * sub_words, "N copies of the subterm");
    assert_eq!(shared_dst.used(), 2 * N + sub_words, "one copy of the subterm");
    assert_eq!(shared_dst.used(), 2 * N + 2 * K);
    assert_eq!(size_of_copy(outer), flat_dst.used());
    // ~1/N: with N = 8 the shared copy is under an eighth of the flat one.
    assert!(
        flat_dst.used() as f64 / shared_dst.used() as f64 > 7.0,
        "the shared copy must be ~1/{N} of the flat size ({} vs {})",
        flat_dst.used(),
        shared_dst.used()
    );

    // (2) The shared copy is still structurally `=:=` the original.
    assert!(compare::eq(outer, flat, &atoms));
    assert!(compare::eq(outer, shared, &atoms));

    // Every one of the N occurrences resolves to the SAME destination object.
    let shared_cars = car_words(shared);
    assert_eq!(shared_cars.len(), N);
    assert!(
        shared_cars.windows(2).all(|w| w[0] == w[1]),
        "all {N} occurrences must share one destination address"
    );
    assert_ne!(shared_cars[0], sub.raw(), "the shared occurrence is a real copy");

    // Contrast: the flat copy materialises N distinct objects.
    let flat_cars = car_words(flat);
    assert_eq!(flat_cars.len(), N);
    assert!(
        flat_cars.windows(2).all(|w| w[0] != w[1]),
        "the flat copy duplicates the subterm"
    );
}

#[test]
fn a_graph_without_sharing_copies_to_the_flat_size() {
    const N: usize = 20;

    let mut src = Heap::new(4 * N * N + 64).expect("source heap");
    // N *distinct* subterms, so there is nothing to share.
    let subs: Vec<Eterm> = (0..N)
        .map(|i| {
            let elems: Vec<Eterm> = (0..N).map(|j| make_small((i * N + j) as i64)).collect();
            build_list(&mut src, &elems)
        })
        .collect();
    let outer = build_list(&mut src, &subs);

    let atoms = AtomTable::new();
    let src_off = OffHeapTable::new();
    let cap = size_of_copy(outer);

    let mut flat_dst = Heap::new(cap).expect("flat destination heap");
    let mut flat_off = OffHeapTable::new();
    let flat = copy_flat(outer, &src_off, &mut flat_dst, &mut flat_off);

    let mut shared_dst = Heap::new(cap).expect("shared destination heap");
    let mut shared_off = OffHeapTable::new();
    let shared = copy_shared(outer, &src_off, &mut shared_dst, &mut shared_off);

    assert_eq!(
        shared_dst.used(),
        flat_dst.used(),
        "a graph with no sharing copies to exactly the flat size"
    );
    assert_eq!(shared_dst.used(), size_of_copy(outer));
    assert!(compare::eq(outer, flat, &atoms));
    assert!(compare::eq(outer, shared, &atoms));

    // Every occurrence is a distinct object in both copies.
    let shared_cars = car_words(shared);
    assert!(shared_cars.windows(2).all(|w| w[0] != w[1]));
}

#[test]
fn a_shared_off_heap_container_is_refcounted_once_per_identity() {
    const N: usize = 8;

    let mut src_off = OffHeapTable::new();
    let slot = src_off.push(Mso::new(0xABCD_u32, 16));
    assert_eq!(src_off.get(slot).map(|h| h.refcount()), Some(1));

    let mut src = Heap::new(64).expect("source heap");
    let container = bin_ref(&mut src, slot);
    let outer = build_list(&mut src, &vec![container; N]);

    let cap = size_of_copy(outer);

    // Contrast case first: the flat copy pushes one destination MSO entry per occurrence.
    let mut flat_dst = Heap::new(cap).expect("flat destination heap");
    let mut flat_off = OffHeapTable::new();
    let _ = copy_flat(outer, &src_off, &mut flat_dst, &mut flat_off);
    assert_eq!(flat_off.len(), N, "the flat copy refcounts the object N times");
    assert_eq!(flat_off.get(0).map(|h| h.refcount()), Some(1 + N as usize));
    let after_flat = src_off.get(slot).map(|h| h.refcount()).expect("held");
    assert_eq!(after_flat, 1 + N);

    // The shared copy materialises the container once: one destination entry, one new reference.
    let mut shared_dst = Heap::new(cap).expect("shared destination heap");
    let mut shared_off = OffHeapTable::new();
    let shared = copy_shared(outer, &src_off, &mut shared_dst, &mut shared_off);

    assert_eq!(shared_off.len(), 1, "one off-heap entry per source identity");
    assert_eq!(
        shared_off.get(0).map(|h| h.refcount()),
        Some(after_flat + 1),
        "the shared identity is refcounted exactly once"
    );
    assert_eq!(
        shared_off.get(0).and_then(|h| h.downcast_ref::<u32>()),
        Some(&0xABCD)
    );

    // All N occurrences of the container share one destination address.
    let shared_cars = car_words(shared);
    assert_eq!(shared_cars.len(), N);
    assert!(shared_cars.windows(2).all(|w| w[0] == w[1]));
}

#[test]
fn immediates_stay_shared_in_both_copies() {
    let mut src = Heap::new(16).expect("source heap");
    let tail = src.cons(make_small(2), NIL).expect("the tail fits");
    let list = src.cons(make_small(1), tail).expect("the head fits");

    let src_off = OffHeapTable::new();
    let mut flat_dst = Heap::new(4).expect("flat destination heap");
    let mut flat_off = OffHeapTable::new();
    let flat = copy_flat(list, &src_off, &mut flat_dst, &mut flat_off);

    let mut shared_dst = Heap::new(4).expect("shared destination heap");
    let mut shared_off = OffHeapTable::new();
    let shared = copy_shared(list, &src_off, &mut shared_dst, &mut shared_off);

    assert_eq!(flat_dst.used(), 4, "two cons cells, immediates shared");
    assert_eq!(shared_dst.used(), flat_dst.used());
    assert!(compare::eq(list, flat, &AtomTable::new()));
    assert!(compare::eq(list, shared, &AtomTable::new()));
    // The cons cells themselves are still fresh objects (only immediates are shared).
    assert_ne!(list.raw(), flat.raw());
    assert_ne!(list.raw(), shared.raw());
}
