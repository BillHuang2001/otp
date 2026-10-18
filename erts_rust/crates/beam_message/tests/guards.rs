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

//! Copy-boundary **guards**: non-term words, forged cycles, and the relocation invariant.
//!
//! Three defensive properties of [`copy_struct`](beam_message::copy_struct) are pinned here;
//! all three are *correctness* properties (never a wrong result, never a hang):
//!
//! 1. **A non-term word is a typed error.** `THE_NON_VALUE` (the C ABI's "no value" marker, a bare
//!    `HEADER` word) and every other stray header word are refused with
//!    [`BeamError::BadArg`](beam_common::BeamError::BadArg) and write **no** destination word -
//!    they are never copied as if they were a bogus leaf. `beam_term::size_object` on such a word
//!    is also pinned to be `0` (a documented `beam_term` hardening: the C dereferences
//!    `boxed_val(0)`).
//! 2. **A forged cycle terminates with a typed error.** A term the engine builds is acyclic, but a
//!    raw write into the heap can point a subterm back at one of its own ancestors.
//!    `beam_term::size_object` walks a graph with an unbounded worklist, so such a cycle would loop
//!    forever; the copier's preflight ([`copy::preflight_words`](beam_message::copy::preflight_words))
//!    instead bounds its own walk and rejects a cyclic graph with `BadArg` - promptly, and having
//!    written nothing. The test *completing* is the assertion.
//! 3. **No live term is left pointing into the source heap.** Every non-shared compound subterm of a
//!    copied graph is a fresh object inside the destination region, while a refc binary's *payload*
//!    is shared by reference (its on-heap pair is relocated, its data pointer is not).
//!
//! The forged cycles are built with raw heap writes; every such write carries a `// SAFETY:`
//! rationale.
//!
//! `beam_term::compare` models every class these assertions meet, including external
//! pid/port/reference (`cmp_external_*`, pinned by `tests/external.rs`) and `RECORD_SUBTAG`
//! (`cmp_records`), so `compare::eq` is the structural check throughout.

use std::collections::HashSet;

use beam_alloc::{Heap, RefcBinary};
use beam_common::BeamError;
use beam_gc::Mso;
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, copy_struct_shared, copy_struct_simple, size_of_copy, CopyContext};
use beam_term::prelude::*;
use beam_term::{compare, size_object};

// ---------------------------------------------------------------------------
// 1. Non-term words (bare header words) are typed errors
// ---------------------------------------------------------------------------

#[test]
fn the_non_value_is_a_typed_error_and_copies_nothing() {
    assert!(is_non_value(THE_NON_VALUE), "the debug-build non-value marker");
    assert!(!is_term_word(THE_NON_VALUE), "a header word is not a term");
    // `size_object` is the canonical sizing entry point and is called by `size_of_copy`; it must
    // neither hang nor panic on a non-term (the C would dereference `boxed_val(0)`).
    assert_eq!(size_object(THE_NON_VALUE), 0);
    assert_eq!(size_of_copy(THE_NON_VALUE), 0);

    let mut dst = Heap::new(8).expect("destination heap");
    match copy_struct_simple(THE_NON_VALUE, &mut dst) {
        Err(BeamError::BadArg) => {}
        other => panic!("expected BadArg for THE_NON_VALUE, got {other:?}"),
    }
    assert_eq!(dst.used(), 0, "the refusal writes no destination word");
}

#[test]
fn stray_header_words_are_typed_errors() {
    // A zero-arity (empty) word, a tuple header with no body, and the thing headers of a float, a
    // BinRef container and a match context: all bare `HEADER` words, none of them a term.
    let words = [
        make_header(0, 0),
        make_header(3, ARITYVAL_SUBTAG),
        HEADER_FLONUM,
        HEADER_BIN_REF,
        HEADER_SUB_BITS,
    ];
    for word in words {
        assert!(!is_term_word(word), "{word:?} must not be a term word");
        assert!(!is_immediate(word));
        assert_eq!(size_object(word), 0, "{word:?} is not a term graph");

        let mut dst = Heap::new(16).expect("destination heap");
        match copy_struct_simple(word, &mut dst) {
            Err(BeamError::BadArg) => {}
            other => panic!("expected BadArg for {word:?}, got {other:?}"),
        }
        assert_eq!(dst.used(), 0, "{word:?} writes no destination word");
    }
}

// ---------------------------------------------------------------------------
// 2. Forged cycles terminate with a typed error
// ---------------------------------------------------------------------------

/// A cons cell whose `cdr` points back at itself: the forged term `[1 | <self>]`.
fn self_referential_cons(heap: &mut Heap) -> Eterm {
    let list = heap.cons(make_small(1), NIL).expect("the cons fits");
    // SAFETY: `heap.cons` just reserved this cell, which the heap exclusively owns; `list` is the
    // tagged pointer to it, so `list_val(list).add(1)` is the cell's own `cdr` word. The write makes
    // the graph cyclic on purpose - exactly the malformed input the cycle guard must survive.
    unsafe { *list_val(list).add(1) = list };
    list
}

/// A one-element tuple whose single element is the tuple itself: `{<self>}`.
fn self_referential_tuple(heap: &mut Heap) -> Eterm {
    // SAFETY: two words are reserved for the 1-tuple; word 0 is its header and word 1 its element,
    // which is set to the tagged pointer to the very same object (a forged cycle).
    unsafe {
        let p = heap.alloc_words(2).expect("the tuple fits");
        *p = make_arityval(1);
        *p.add(1) = make_boxed(p as *const Eterm);
        make_boxed(p as *const Eterm)
    }
}

/// Two cons cells pointing at each other: `a = [1 | b]`, `b = [2 | a]`.
fn mutual_cons_cycle(heap: &mut Heap) -> Eterm {
    let a = heap.cons(make_small(1), NIL).expect("cell a fits");
    let b = heap.cons(make_small(2), a).expect("cell b fits");
    // SAFETY: both cells were just reserved and are exclusively owned by the heap; `b` is the tagged
    // pointer to the second, so `list_val(b).add(1)` is its `cdr`. Pointing `a`'s `cdr` at `b`
    // closes the two-cell cycle.
    unsafe { *list_val(a).add(1) = b };
    a
}

/// Asserts that copying the cyclic `term` fails with a typed `BadArg` on a destination of
/// `capacity` words, writing nothing. The test returning at all is the "never hangs" evidence.
fn assert_cycle_is_refused(term: Eterm, capacity: usize) {
    let mut dst = Heap::new(capacity).expect("destination heap");
    match copy_struct_simple(term, &mut dst) {
        Err(BeamError::BadArg) => {}
        other => panic!("expected BadArg for a forged cycle, got {other:?}"),
    }
    assert_eq!(dst.used(), 0, "a refused copy writes no destination word");
}

#[test]
fn a_forged_cons_cycle_is_refused_small_and_large_destinations() {
    // The guard must not depend on the destination being too small: with a tiny heap the bounded
    // walk overflows immediately, and with a huge one it is bounded by the heap's slack alone.
    for capacity in [4usize, 4096] {
        let mut src = Heap::new(64).expect("source heap");
        assert_cycle_is_refused(self_referential_cons(&mut src), capacity);
    }
}

#[test]
fn a_forged_tuple_cycle_is_refused() {
    let mut src = Heap::new(64).expect("source heap");
    assert_cycle_is_refused(self_referential_tuple(&mut src), 4096);
}

#[test]
fn a_forged_two_cell_cycle_is_refused() {
    let mut src = Heap::new(64).expect("source heap");
    assert_cycle_is_refused(mutual_cons_cycle(&mut src), 4096);
}

#[test]
fn a_cycle_reached_through_a_valid_prefix_is_refused() {
    // A perfectly ordinary 100-cell list whose last `cdr` is forged into a cycle: the copier must
    // notice before descending forever.
    let mut src = Heap::new(256).expect("source heap");
    let mut acc = NIL;
    let mut cells = Vec::new();
    for i in 0..100 {
        acc = src.cons(make_small(i), acc).expect("the cons fits");
        cells.push(acc);
    }
    // Close the loop: the outermost cell's `cdr` points back at the innermost cell (a long cycle).
    let innermost = *cells.last().expect("at least one cell");
    // SAFETY: `acc` is the most recent (outermost) cell, exclusively owned by the heap; its `cdr`
    // word is rewritten to point at the innermost cell, forging a 100-cell cycle.
    unsafe { *list_val(acc).add(1) = innermost };

    assert_cycle_is_refused(acc, 4096);
}

#[test]
fn the_sharing_preserving_copier_also_refuses_a_forged_cycle() {
    // `copy_struct_shared` shares the same preflight, so it must terminate identically.
    let mut src = Heap::new(64).expect("source heap");
    let cyclic = self_referential_cons(&mut src);

    let mut dst = Heap::new(64).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let src_off = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src_off, &mut dst_off);
    match copy_struct_shared(cyclic, &mut dst, &mut ctx) {
        Err(BeamError::BadArg) => {}
        other => panic!("expected BadArg, got {other:?}"),
    }
    assert_eq!(dst.used(), 0);
}

#[test]
fn a_deep_acyclic_list_is_not_mistaken_for_a_cycle() {
    // The guard must not refuse a legitimately deep graph: 20_000 cells exceed any small destination
    // and are refused only by *capacity* (a message allocation failure), never as `BadArg`.
    const N: usize = 20_000;
    let mut src = Heap::new(2 * N + 16).expect("source heap");
    let mut acc = NIL;
    for i in 0..N {
        acc = src.cons(make_small((i % 97) as i64), acc).expect("the cons fits");
    }
    assert_eq!(size_of_copy(acc), 2 * N);

    let mut dst = Heap::new(2 * N).expect("destination heap");
    let copy = copy_struct_simple(acc, &mut dst).expect("a deep acyclic list copies");
    assert!(compare::eq(acc, copy, &AtomTable::new()));
    assert_eq!(dst.used(), 2 * N);
}

// ---------------------------------------------------------------------------
// 3. No live term is left pointing into the source heap
// ---------------------------------------------------------------------------

/// The address of the object a non-shared compound term points at.
fn object_addr(term: Eterm) -> usize {
    if is_list(term) {
        list_val(term) as usize
    } else {
        boxed_val(term) as usize
    }
}

/// Whether `addr` lies inside `heap`'s region `[base, hend)`.
fn heap_contains(heap: &Heap, addr: usize) -> bool {
    let base = heap.hp() as usize - heap.used() * core::mem::size_of::<Eterm>();
    addr >= base && addr < heap.hend() as usize
}

/// Whether `term` is shared as-is by the copier: an immediate, or a literal-area compound term (the
/// shared zero-arity tuple is one such).
fn is_shared_term(term: Eterm) -> bool {
    is_immediate(term) || ((is_boxed(term) || is_list(term)) && is_literal(term))
}

/// Asserts that every non-shared compound subterm reachable from `root` is a fresh object **inside**
/// `dst` - i.e. nothing in the copied graph still points into the source heap.
///
/// The walk mirrors the copier's own descent (cons cells, tuple/record elements, a fun's
/// environment, flatmap keys/values, HAMT node slots), stopping at the two off-heap references: an
/// MSO container's `val` is a slot index rather than a term, and a refc binary's payload is shared
/// by reference (only its on-heap `[BinRef][ErlSubBits]` pair must be relocated).
fn assert_all_relocated(root: Eterm, dst: &Heap) {
    let mut visited: HashSet<usize> = HashSet::new();
    let mut stack = vec![root];
    while let Some(term) = stack.pop() {
        if is_shared_term(term) {
            continue;
        }
        assert!(
            is_boxed(term) || is_list(term),
            "a copied subterm must be boxed or a list, not {term:?}"
        );
        let addr = object_addr(term);
        assert!(
            heap_contains(dst, addr),
            "a copied subterm lives at {addr:#x}, outside the destination region"
        );
        if !visited.insert(addr) {
            continue;
        }
        if is_list(term) {
            // SAFETY: `term` is a live cons cell, so both words are readable.
            let (head, tail) = unsafe { (car(term), cdr(term)) };
            stack.push(head);
            stack.push(tail);
            continue;
        }
        // SAFETY: `term` is a live boxed term, so its header word is readable.
        let header = unsafe { *boxed_val(term) };
        // An off-heap container's `val` is an MSO slot index, not a term: it exposes no child.
        if header.raw() == HEADER_BIN_REF.raw() {
            continue;
        }
        // SAFETY: `term` is a live boxed term.
        if unsafe { is_refc_binary(term) } {
            // SAFETY: the pair's `BinRef` sits `ERL_BIN_REF_SIZE` words below the `ErlSubBits` root.
            let pair = unsafe { boxed_val(term).sub(ERL_BIN_REF_SIZE) };
            assert!(
                heap_contains(dst, pair as usize),
                "the copied BinRef must be inside the destination region"
            );
            // The payload pointer is deliberately NOT checked: it is shared with the source.
            continue;
        }
        let p = boxed_val(term);
        match header.raw() & TAG_HEADER_MASK {
            ARITYVAL_SUBTAG | RECORD_SUBTAG => {
                for offset in 1..=header_arity(header) {
                    // SAFETY: `offset` is an in-bounds word of the object at `p`.
                    stack.push(unsafe { *p.add(offset) });
                }
            }
            FUN_SUBTAG => {
                // `env[i]` lives at `1 + desc_words(kind) + i` (the frozen `beam_term::fun` layout).
                let env = 1 + fun_desc_words(fun_kind_of_header(header));
                for offset in env..env + fun_num_free(header) {
                    // SAFETY: the env slots are in-bounds words of the fun block at `p`.
                    stack.push(unsafe { *p.add(offset) });
                }
            }
            MAP_SUBTAG if is_flatmap_header(header) => {
                // SAFETY: `p` heads a live flatmap; word 1 is its untagged size word.
                let size = unsafe { *p.add(1) }.raw();
                // SAFETY: word 2 is the keys tuple and words 3..3+size the values.
                unsafe { stack.push(*p.add(2)) };
                for offset in 3..3 + size {
                    // SAFETY: as above.
                    stack.push(unsafe { *p.add(offset) });
                }
            }
            MAP_SUBTAG => {
                // A HAMT node: one slot per set occupancy bit, after the head kinds' `size` word.
                let arity = header_arity(header);
                let nslots = map_header_val(header).count_ones() as usize;
                for i in 0..nslots {
                    // SAFETY: slot `i` is an in-bounds word of the node at `p`.
                    stack.push(unsafe { *p.add(1 + arity + i) });
                }
            }
            // A leaf thing (float/bignum/ref/heap bits/external pid/port/ref): no navigable child.
            _ => {}
        }
    }
}

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first.
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the list fits");
    }
    acc
}

/// Builds a `Local` (`make_fun3`) closure block on `heap`.
fn make_local_fun(heap: &mut Heap, arity: usize, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
    // SAFETY: `fun_words(Local, env.len())` is the block's exact size and `write_fun_local` writes
    // exactly those words; every `env` term is live.
    unsafe {
        let hp = heap
            .alloc_words(fun_words(FunKind::Local, env.len()))
            .expect("the fun block fits");
        write_fun_local(hp, arity, uniq, index, env).expect("a valid local fun")
    }
}

/// Builds an off-heap MSO reference container `[HEADER_BIN_REF, slot, 0]` on `heap`.
fn bin_ref(heap: &mut Heap, slot: usize) -> Eterm {
    // SAFETY: the heap owns these three words.
    unsafe {
        let p = heap.alloc_words(ERL_BIN_REF_SIZE).expect("the container fits");
        *p = HEADER_BIN_REF;
        *p.add(1) = Eterm::from_raw(slot);
        *p.add(2) = Eterm::from_raw(0);
        make_boxed(p as *const Eterm)
    }
}

/// Builds a `SUB_BITS` **match context** over a fresh on-heap bitstring on `heap`: a window
/// starting at a NON-byte-aligned bit offset, whose `orig` is that on-heap bitstring.
fn match_context(heap: &mut Heap) -> Eterm {
    let bytes = [0x5Au8; 9];
    let hb_bits = bytes.len() * 8;
    // SAFETY: `heap_bits_size_words(hb_bits)` words are reserved and the payload fits.
    let hb = unsafe {
        let hp = heap.alloc_words(heap_bits_size_words(hb_bits)).expect("fits");
        write_heap_bits(hp, &bytes, hb_bits).expect("the payload fits")
    };
    let start = 3usize;
    let end = start + 21;
    // SAFETY: the on-heap bitstring's data base is word-aligned, `hb`'s payload covers bits
    // `start..end`, and 5 words are reserved for the `ErlSubBits`.
    unsafe {
        let base = bitstring_parts(hb).0;
        let hp = heap.alloc_words(5).expect("the match context fits");
        write_sub_bits(
            hp,
            Eterm::from_raw(base as usize),
            Eterm::from_raw(start),
            Eterm::from_raw(end),
            hb,
        )
    }
}

/// Writes a flatmap (or HAMT map, past `MAP_SMALL_MAP_LIMIT`) on `heap`.
fn write_map_term(heap: &mut Heap, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Eterm {
    // SAFETY: `map_words_capacity` is a sound upper bound on the words `write_map` writes.
    unsafe {
        let hp = heap
            .alloc_words(map_words_capacity(pairs.len()))
            .expect("the map fits");
        write_map(hp, pairs, atoms).expect("the map builds")
    }
}

#[test]
fn no_live_term_is_left_pointing_into_the_source_heap() {
    let atoms = AtomTable::new();
    let mut src = Heap::new(8192).expect("source heap");

    let nested = src.tuple(&[make_small(1), make_small(2)]).expect("fits");
    let list = build_list(&mut src, &[nested, make_small(3), nested]);
    let flat_pairs: Vec<(Eterm, Eterm)> = (0..8)
        .map(|i| (make_small(i), make_small(i * 10 + 1)))
        .collect();
    let flat_map = write_map_term(&mut src, &flat_pairs, &atoms);
    let hamt_pairs: Vec<(Eterm, Eterm)> = (0..64)
        .map(|i| (make_small(i * 3), make_small(i * 1000 + 7)))
        .collect();
    let hamt_map = write_map_term(&mut src, &hamt_pairs, &atoms);
    let fun_env = src.tuple(&[make_small(11), list]).expect("the env fits");
    let fun = make_local_fun(&mut src, 2, 0xDEAD_BEEF_0000_0001, 7, &[fun_env]);

    // A `SUB_BITS` match context: `copy_struct` must *flatten* it into an on-heap bitstring inside
    // the destination (its `base_flags`/`orig` would otherwise still point into `src`).
    let mc = match_context(&mut src);

    let root = src
        .tuple(&[nested, list, flat_map, hamt_map, fun, mc])
        .expect("the root tuple fits");

    let cap = size_of_copy(root) + 16;
    let mut dst = Heap::new(cap).expect("destination heap");
    let copy = copy_struct_simple(root, &mut dst).expect("the copy succeeds");

    // A genuine copy: distinct root, and structurally the same graph.
    assert_ne!(object_addr(root), object_addr(copy));
    assert!(compare::eq(root, copy, &atoms), "the copy is =:= the original");

    // Every compound subterm lives in the destination; nothing points back into the source.
    assert_all_relocated(copy, &dst);

    // The walk is not vacuous: the destination really did receive the whole graph.
    assert_eq!(dst.used(), size_of_copy(root));
}

#[test]
fn an_off_heap_container_is_relocated_while_its_object_is_shared() {
    let mut src_off = OffHeapTable::new();
    // A filler at slot 0, so the interesting handle lands at slot 1: the source slot then differs
    // from the destination slot and the rewrite is observable.
    let _filler = src_off.push(Mso::new(0_u32, 4));
    let slot = src_off.push(Mso::new(0x5A5A_u32, 12));
    assert_eq!(slot, 1);

    let mut src = Heap::new(64).expect("source heap");
    let container = bin_ref(&mut src, slot);
    let root = src.tuple(&[container, make_small(1)]).expect("the tuple fits");

    let mut dst = Heap::new(size_of_copy(root) + 8).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        copy_struct(root, &mut dst, &mut ctx).expect("the copy succeeds")
    };

    // The container itself is relocated (an MSO slot index is not a term, so the walker stops there).
    assert_all_relocated(copy, &dst);
    assert_eq!(dst.used(), 3 + ERL_BIN_REF_SIZE, "the 2-tuple and the container");

    // SAFETY: `copy` is a live 2-tuple; element 0 is the copied container.
    let copied_container = unsafe { tuple_element(copy, 0) };
    assert!(is_boxed(copied_container));
    // SAFETY: `copied_container` is a live `BinRef` thing, so word 1 is its `val`.
    let dst_slot = unsafe { (*boxed_val(copied_container).add(1)).raw() };
    assert_ne!(dst_slot, slot, "the slot is rewritten to the destination table's slot");
    assert_eq!(dst_slot, 0, "the destination table's first entry");    assert_eq!(dst_off.len(), 1, "the source handle is refcounted into the destination");
    assert_eq!(dst_off.get(0).map(|h| h.refcount()), Some(2), "source + destination");
    assert_eq!(
        dst_off.get(0).and_then(|h| h.downcast_ref::<u32>()),
        Some(&0x5A5A),
        "the shared MSO object carries the source payload"
    );
}

#[test]
fn a_refc_binary_is_relocated_while_its_payload_is_shared() {
    let bytes: Vec<u8> = (0..200usize).map(|i| ((i * 13) % 251) as u8).collect();
    let mut src_off = OffHeapTable::new();
    src_off.push_binary(RefcBinary::new(&bytes).expect("the payload allocates"));
    let data_ptr = src_off.binaries()[0].data_ptr();

    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: 8 words are reserved for the pair; `src_off` owns the payload (aligned by
    // `RefcBinary`), which outlives the term.
    let pair = unsafe {
        let hp = src.alloc_words(ERL_REFC_BITS_SIZE).expect("the pair fits");
        write_refc_binary(
            hp,
            data_ptr as *mut Eterm,
            core::ptr::null_mut(),
            data_ptr,
            0,
            bytes.len() * 8,
        )
    };
    let root = src.tuple(&[pair]).expect("the tuple fits");

    let mut dst = Heap::new(size_of_copy(root) + 8).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        copy_struct(root, &mut dst, &mut ctx).expect("the copy succeeds")
    };

    // The on-heap pair is relocated into the destination; the payload pointer is shared.
    assert_all_relocated(copy, &dst);
    // SAFETY: `copy` is a live 1-tuple whose element is the copied refc binary.
    let copied = unsafe { tuple_element(copy, 0) };
    assert!(unsafe { is_refc_binary(copied) });
    // SAFETY: `copied` is a live refc binary, so its `BinRef` sits `ERL_BIN_REF_SIZE` words below.
    let copied_bin_ref = unsafe { boxed_val(copied).sub(ERL_BIN_REF_SIZE) };
    assert!(
        heap_contains(&dst, copied_bin_ref as usize),
        "the copied BinRef is inside the destination"
    );
    // SAFETY: `copied_bin_ref` is the copied `BinRef` head, so word 1 is its `val`.
    assert_eq!(
        unsafe { (*copied_bin_ref.add(1)).raw() },
        data_ptr as usize,
        "the payload pointer is the source's - the payload is shared, not deep-copied"
    );
    assert!(
        !heap_contains(&dst, data_ptr as usize),
        "the 200-byte payload lives off-heap in the source's `RefcBinary`"
    );
    assert_eq!(unsafe { refc_bytes(copied) }, &bytes[..]);
}
