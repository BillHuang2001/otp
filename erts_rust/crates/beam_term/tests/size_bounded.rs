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

//! `size_object_bounded` — the bounded, cycle-safe sizing walk.
//!
//! `size_object` ports `size_object_x(obj, NULL)` (`erts/emulator/beam/copy.c`),
//! which walks the term graph unconditionally: on a **cyclic** graph — a compound
//! object reachable from itself, which only forged memory can produce — the C
//! recurses forever. `size_object_bounded` exposes the same walk with a word budget
//! and reports *why* a graph cannot be sized:
//!
//! | graph | `size_object_bounded(a, limit)` |
//! |---|---|
//! | acyclic, `n <= limit` | `Ok(n)` (identical to `size_object(a)`) |
//! | acyclic, `n > limit` | `Err(TermError::TermGraphTooLarge)` |
//! | cyclic | `Err(TermGraphCyclic)` — **always**, even if the budget ran out first |
//!
//! and `size_object` itself keeps `size_object_x`'s exact answer for every acyclic
//! term, returning a defined partial count (never hanging) for a cyclic one.
//!
//! Note on the guard: the only words this walk refuses to dereference are those
//! whose primary tag is `HEADER` (`THE_NON_VALUE` and friends — see
//! [`is_term_word`]). A word tagged `BOXED`/`LIST` that points outside any heap
//! cannot be tested without invoking UB — detecting it needs a heap registry this
//! crate deliberately does not have — so the non-term coverage below uses forged
//! `HEADER`-tagged words.

use beam_term::prelude::*;

/// `n` distinct small-integer keys with small-integer values (immediates, so only
/// the map's own words are ever counted).
fn int_pairs(n: i64) -> Vec<(Eterm, Eterm)> {
    (1..=n)
        .map(|k| (make_small(k), make_small(k * 3)))
        .collect()
}

/// A map laid out by [`write_map`] into a buffer that outlives the returned term.
struct BuiltMap {
    _buf: Vec<Eterm>,
    map: Eterm,
}

/// Lays `pairs` out with [`write_map`] into a buffer of exactly
/// [`map_words_capacity`]`(pairs.len())` words.
fn build_map(pairs: &[(Eterm, Eterm)]) -> BuiltMap {
    let atoms = AtomTable::new();
    let cap = map_words_capacity(pairs.len());
    let mut buf = vec![Eterm::from_raw(0xDEAD_BEEF); cap];
    // SAFETY: `buf` owns the `cap` words `write_map` needs (its exact capacity) and
    // is kept alive by the returned value for as long as the map is used.
    let map = unsafe { write_map(buf.as_mut_ptr(), pairs, &atoms) }.unwrap();
    BuiltMap { _buf: buf, map }
}

#[test]
fn a_valid_term_sized_at_and_over_its_budget() {
    let mut heap = Heap::new(64);
    let tuple = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
    let tail = heap.cons(make_small(9), NIL).unwrap();
    let list = heap.cons(make_small(8), tail).unwrap();
    let shared = heap
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .unwrap();
    let outer = heap.tuple(&[shared, shared, list]).unwrap();

    for t in [
        make_small(7),
        NIL,
        make_atom(Atom::from_raw(1)),
        tuple0(),
        tuple,
        list,
        outer,
    ] {
        let n = size_object(t);
        // The budget is inclusive: exactly `n` words fits.
        assert_eq!(size_object_bounded(t, n), Ok(n), "{t:?} at its own size");
        // One word short: a well-formed graph, but over budget.
        if n == 0 {
            assert_eq!(size_object_bounded(t, 0), Ok(0), "{t:?} is 0 words");
        } else {
            assert_eq!(
                size_object_bounded(t, n - 1),
                Err(TermError::TermGraphTooLarge),
                "{t:?} one word over budget"
            );
        }
        // A generous budget never changes the answer.
        assert_eq!(size_object_bounded(t, usize::MAX), Ok(n), "{t:?}");
    }
}

#[test]
fn a_cyclic_cons_cell_is_a_typed_error_not_a_hang() {
    let mut heap = Heap::new(4);
    // `L = [1 | L]`: a cons cell whose `cdr` is the cell itself.
    // SAFETY: 2 words are reserved for the cons cell and both are written before the
    // term is handed to any accessor.
    let p = unsafe { heap.alloc_words(2).unwrap() };
    let cell = make_list(p);
    unsafe {
        *p = make_small(1);
        *p.add(1) = cell;
    }
    assert!(is_list(cell));

    // With a budget that is never reached, the walk stops at the back-edge.
    assert_eq!(
        size_object_bounded(cell, usize::MAX),
        Err(TermError::TermGraphCyclic)
    );
    assert_eq!(
        size_object_bounded(cell, 1_000),
        Err(TermError::TermGraphCyclic)
    );
    // The budget *is* spent before the back-edge here (the cell alone is 2 words), yet
    // "cyclic" still wins over "too large": the error path runs the acyclicity pass.
    for limit in [0usize, 1] {
        assert_eq!(
            size_object_bounded(cell, limit),
            Err(TermError::TermGraphCyclic),
            "limit {limit}"
        );
    }

    // `size_object` terminates and returns the documented partial count: the root
    // cons cell (2 words) counted before the edge that closed the cycle.
    assert_eq!(size_object(cell), 2);
}

#[test]
fn a_cycle_is_detected_through_every_object_kind() {
    // (a) A two-cell cycle: `A = [1 | B]`, `B = [2 | A]`.
    let mut heap = Heap::new(8);
    // SAFETY: both cells are fully written before use.
    let (pa, pb) = unsafe { (heap.alloc_words(2).unwrap(), heap.alloc_words(2).unwrap()) };
    let (a, b) = (make_list(pa), make_list(pb));
    unsafe {
        *pa = make_small(1);
        *pa.add(1) = b;
        *pb = make_small(2);
        *pb.add(1) = a;
    }
    assert_eq!(
        size_object_bounded(a, usize::MAX),
        Err(TermError::TermGraphCyclic)
    );
    assert_eq!(size_object(a), 4, "both cells counted before the back-edge");

    // (b) A tuple that contains itself: `{T}` with `T` in its only element slot.
    let mut heap = Heap::new(4);
    // SAFETY: the header and the single element are written before use.
    let pt = unsafe { heap.alloc_words(2).unwrap() };
    let t = make_boxed(pt);
    unsafe {
        *pt = make_arityval(1);
        *pt.add(1) = t;
    }
    assert_eq!(
        size_object_bounded(t, usize::MAX),
        Err(TermError::TermGraphCyclic)
    );
    assert_eq!(size_object(t), 2, "header + payload, then the self-edge");

    // (c) A fun whose free variable is the fun itself (`1 + 2 + 1` words).
    let mut heap = Heap::new(8);
    // SAFETY: `fun_words` sizes the block exactly and the heap owns it.
    let fun = unsafe {
        let hp = heap.alloc_words(fun_words(FunKind::Local, 1)).unwrap();
        write_fun_local(hp, 1, 0x1234, 0, &[make_small(0)]).unwrap()
    };
    // SAFETY: `fun` is a live local fun; the offset is its own env offset in-block.
    let env_off = fun_env_offset_header(unsafe { *boxed_val(fun) }).unwrap();
    // SAFETY: `env_off` addresses the fun's single free-variable slot.
    unsafe { *boxed_val(fun).add(env_off) = fun };
    assert_eq!(
        size_object_bounded(fun, usize::MAX),
        Err(TermError::TermGraphCyclic)
    );
    assert_eq!(size_object(fun), 4);

    // (d) A HAMT (map) node whose one slot holds the node itself.
    let mut heap = Heap::new(8);
    // SAFETY: `[hdr, size, slot0]` is 3 words, all written before use.
    let pm = unsafe { heap.alloc_words(3).unwrap() };
    let node = make_boxed(pm);
    unsafe {
        *pm = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 0b0001);
        *pm.add(1) = Eterm::from_raw(1); // the head node's key-count `size` word
        *pm.add(2) = node;
    }
    assert_eq!(
        size_object_bounded(node, 1_000),
        Err(TermError::TermGraphCyclic)
    );
    assert_eq!(size_object(node), 3, "`1 + popcount(bitmap) + arity` words");
}

#[test]
fn a_cycle_behind_a_long_prefix_still_wins_over_too_large() {
    // A chain of `N` cells whose last cell's `cdr` is the first cell. The budget is
    // spent well before the back-edge is reached, so only the error-path acyclicity
    // pass can see the cycle — and it must, because "cyclic" outranks "too large".
    const N: usize = 1_000;
    let mut heap = Heap::new(2 * N);
    let head = heap.cons(make_small(0), NIL).unwrap();
    let mut tail = head;
    for i in 1..N {
        tail = heap.cons(make_small(i as i64), tail).unwrap();
    }
    // SAFETY: `head` is a live cons cell of this heap; patching its `cdr` word closes
    // the cycle back to the chain head.
    unsafe { *list_val(head).add(1) = tail };

    assert_eq!(
        size_object_bounded(tail, 100),
        Err(TermError::TermGraphCyclic),
        "the budget is spent long before the back-edge"
    );
    assert_eq!(
        size_object_bounded(tail, usize::MAX),
        Err(TermError::TermGraphCyclic)
    );
    assert_eq!(size_object(tail), 2 * N, "every cell up to the back-edge");
}

#[test]
fn non_terms_contribute_zero_words_and_are_never_dereferenced() {
    let forged = [
        THE_NON_VALUE,
        make_header(0, FLOAT_SUBTAG),
        HEADER_FLONUM,
        Eterm::from_raw(0xDEAD_BEEF_0000_0000),
        Eterm::from_raw(0x3C),
    ];
    for nt in forged {
        assert!(is_header(nt), "{nt:?} must be a bare header word");
        assert!(!is_term_word(nt), "{nt:?} is not a term word");
        assert_eq!(size_object(nt), 0, "{nt:?}");
        assert_eq!(size_object_bounded(nt, 0), Ok(0), "{nt:?}");
        assert_eq!(size_object_bounded(nt, usize::MAX), Ok(0), "{nt:?}");
    }

    // A non-term *slot* inside an otherwise valid container contributes 0 too.
    let mut heap = Heap::new(8);
    let t = heap.tuple(&[THE_NON_VALUE, make_small(1)]).unwrap();
    assert_eq!(size_object(t), 3, "arity 2 => 3 words; the non-term adds 0");
    assert_eq!(size_object_bounded(t, 3), Ok(3));
    assert_eq!(size_object_bounded(t, 2), Err(TermError::TermGraphTooLarge));
}

#[test]
fn a_deep_list_is_sized_iteratively_without_a_stack_overflow() {
    const N: usize = 100_000;
    let mut heap = Heap::new(2 * N + 3);
    let mut list = NIL;
    for i in 0..N {
        list = heap.cons(make_small(i as i64), list).unwrap();
    }
    let expected = 2 * N;

    assert_eq!(size_object(list), expected);
    assert_eq!(size_object_bounded(list, expected), Ok(expected));
    assert_eq!(
        size_object_bounded(list, expected - 1),
        Err(TermError::TermGraphTooLarge)
    );

    // The same list reached twice through a tuple is counted once **per occurrence**,
    // and is still acyclic (a global "seen" set would wrongly report a cycle or
    // under-count it).
    let pair = heap.tuple(&[list, list]).unwrap();
    let pair_words = 3 + 2 * expected;
    assert_eq!(size_object(pair), pair_words);
    assert_eq!(size_object_bounded(pair, pair_words), Ok(pair_words));
    assert_eq!(
        size_object_bounded(pair, pair_words - 1),
        Err(TermError::TermGraphTooLarge)
    );
}

#[test]
fn a_shared_acyclic_subterm_is_counted_once_per_occurrence() {
    // Three sibling references to the same 3-word tuple: `size_object_x` counts it
    // three times, so the path-based cycle check must not mistake the second and
    // third occurrence for back-edges.
    let mut heap = Heap::new(32);
    let shared = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
    let outer = heap.tuple(&[shared, shared, shared]).unwrap();

    assert_eq!(size_object(shared), 3);
    assert_eq!(size_object(outer), 4 + 3 * 3);
    assert_eq!(size_object_bounded(outer, 13), Ok(13));
    // Over budget: the acyclicity pass walks the *same* DAG and must still find no
    // cycle (it skips the already-finished subterm rather than calling it a cycle).
    assert_eq!(
        size_object_bounded(outer, 12),
        Err(TermError::TermGraphTooLarge)
    );

    // Nested sharing too: `{T, {T, T}}` — one occurrence under the first sibling and
    // two under the second, so the shared tuple is off the DFS path every time it is
    // reached again. `{T,{T,T}}` = 3 (nested) + 3 (T) + 3 (inner) + 3 + 3.
    let inner = heap.tuple(&[shared, shared]).unwrap();
    let nested = heap.tuple(&[shared, inner]).unwrap();
    assert_eq!(size_object(nested), 15);
    assert_eq!(size_object_bounded(nested, 15), Ok(15));
    assert_eq!(
        size_object_bounded(nested, 14),
        Err(TermError::TermGraphTooLarge)
    );
}

#[test]
fn maps_are_sized_exactly_and_respect_the_budget() {
    // A non-empty flat map of `n` pairs is `2n + 4` words (`n + 3`-word body plus the
    // `n + 1`-word keys tuple); the empty one is the special 3-word block.
    for n in [0i64, 1, 3, 32] {
        let b = build_map(&int_pairs(n));
        assert!(
            // SAFETY: `b.map` is a live map term built by `write_map`.
            unsafe { is_flatmap(b.map) },
            "{n} keys must stay a flat map"
        );
        let expected = if n == 0 { 3 } else { 4 + 2 * n as usize };
        assert_eq!(size_object(b.map), expected, "flat map of {n} keys");
        assert_eq!(size_object_bounded(b.map, expected), Ok(expected));
        assert_eq!(
            size_object_bounded(b.map, expected - 1),
            Err(TermError::TermGraphTooLarge)
        );
        assert_eq!(size_object_bounded(b.map, usize::MAX), Ok(expected));
    }

    // 33 keys is the smallest hash map: the whole trie is walked. Its exact size is
    // pinned by `src/size.rs`'s `hashmap_trie_is_counted_in_full` against an
    // independent by-hand node walk; here the point is that both entry points agree
    // and that the budget is exact.
    let b = build_map(&int_pairs(33));
    assert!(
        // SAFETY: `b.map` is a live map term built by `write_map`.
        !unsafe { is_flatmap(b.map) },
        "33 keys must be a hash map"
    );
    let n = size_object(b.map);
    assert!(n > 4 + 2 * 33, "a HAMT stores more than the flat form");
    assert_eq!(size_object_bounded(b.map, n), Ok(n));
    assert_eq!(
        size_object_bounded(b.map, n - 1),
        Err(TermError::TermGraphTooLarge)
    );
    assert_eq!(size_object_bounded(b.map, usize::MAX), Ok(n));
}

#[test]
fn the_budget_is_inclusive_and_a_leaf_never_exceeds_zero() {
    // A leaf compound object is exactly its own words: `{1}` is 2.
    let mut heap = Heap::new(4);
    let t = heap.tuple(&[make_small(1)]).unwrap();
    assert_eq!(size_object(t), 2);
    assert_eq!(size_object_bounded(t, 2), Ok(2));
    assert_eq!(size_object_bounded(t, 1), Err(TermError::TermGraphTooLarge));
    // `tuple0()` is the shared global literal: it is a valid term of size 0, and its
    // sharedness is not a cycle.
    assert_eq!(size_object_bounded(tuple0(), 0), Ok(0));
    assert_eq!(size_object(tuple0()), 0);
}
