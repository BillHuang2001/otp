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
//! `copy.rs`'s unit tests — the copy engine's own pins: the shared-term predicate, the per-class
//! shallow rules, the cons-chain walk (every list shape, a shared literal child, an off-heap child
//! per occurrence) and the chain walk's capacity bound on a forged cyclic `cdr`.
//!
//! A submodule of `copy` rather than an integration suite, so the pins may reach the `pub(crate)`
//! items (`is_shared`, `Fixup`, the worklist) that `tests/` cannot; the crate-level suites for the
//! same behaviour through the public seam live in `tests/copy.rs`, `tests/classes.rs`,
//! `tests/sizing.rs` and `tests/guards.rs`.
use beam_alloc::Heap;
use beam_common::BeamError;
use beam_gc::Mso;
use beam_term::prelude::*;

use super::{copy_struct, copy_struct_simple, is_shared, size_of_copy, CopyContext};
use crate::layout;
use crate::off_heap::OffHeapTable;

/// Builds the list `[e0, e1, …]` on `heap`, tail-first.
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the cons fits");
    }
    acc
}

/// Copies `term` into a fresh, exactly-sized destination and returns the copy plus the heap it
/// lives on (the heap must outlive the caller's use of the copy).
fn copy_exact(term: Eterm) -> (Eterm, Heap) {
    let mut dst = Heap::new(size_of_copy(term)).expect("the destination heap");
    let copy = copy_struct_simple(term, &mut dst).expect("the copy fits exactly");
    (copy, dst)
}

#[test]
fn immediates_and_literals_are_shared() {
    let mut heap = Heap::new(8).expect("test heap");

    assert!(is_shared(make_small(1)));
    assert!(is_shared(NIL));

    let tuple = heap.tuple(&[make_small(1)]).expect("the tuple fits");
    assert!(!is_shared(tuple), "a heap tuple is copied");
    assert!(is_shared(make_literal(tuple)), "a literal-tagged tuple is shared");

    let list = heap.cons(make_small(1), NIL).expect("the cons fits");
    assert!(!is_shared(list), "a heap cons is copied");
    assert!(is_shared(make_literal(list)), "a literal-tagged cons is shared");
}

#[test]
fn zero_arity_tuple_normalises_to_the_shared_literal() {
    let mut heap = Heap::new(8).expect("test heap");
    // SAFETY: the heap owns this one word.
    let p = unsafe { heap.alloc_words(1).expect("the header fits") };
    // SAFETY: `p` is that word; a zero-arity tuple header is a single word.
    unsafe { *p = make_arityval_zero() };
    let t0 = make_boxed(p as *const Eterm);

    let mut dst = Heap::new(8).expect("destination heap");
    let copy = copy_struct_simple(t0, &mut dst).expect("the copy succeeds");
    assert_eq!(copy.raw(), tuple0().raw());
    assert_eq!(dst.used(), 0, "the shared literal uses no destination word");
}

#[test]
fn over_capacity_simple_copy_is_rejected_before_allocating() {
    let mut src = Heap::new(16).expect("source heap");
    let tuple = src
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("the tuple fits");
    let mut dst = Heap::new(1).expect("too-small heap");
    assert!(copy_struct_simple(tuple, &mut dst).is_err());
    assert_eq!(dst.used(), 0);
}

// -----------------------------------------------------------------------
// The cons-chain walk (`copy_cons_chain`)
// -----------------------------------------------------------------------

/// The shapes the chain walk must reproduce cell for cell: a plain integer list, a char list, an
/// improper tail (`[1,2|3]`, which ends the walk on a non-cons), a one-cell list whose tail is
/// `NIL`, and a nested list (each `car` a cons, so every cell records a fix-up).
#[test]
fn a_cons_chain_copies_every_list_shape_identically() {
    let mut src = Heap::new(256).expect("source heap");
    let ints = build_list(&mut src, &(1..=10).map(make_small).collect::<Vec<_>>());
    let chars = build_list(
        &mut src,
        &"a char list"
            .bytes()
            .map(|b| make_small(i64::from(b)))
            .collect::<Vec<_>>(),
    );
    let inner = src.cons(make_small(2), make_small(3)).expect("the cons fits");
    let improper = src.cons(make_small(1), inner).expect("the cons fits");
    let single = src.cons(make_small(7), NIL).expect("the cons fits");
    let a = build_list(&mut src, &[make_small(1), make_small(2)]);
    let b = build_list(&mut src, &[make_small(3), make_small(4)]);
    let nested = {
        let tail = src.cons(b, NIL).expect("the cons fits");
        src.cons(a, tail).expect("the cons fits")
    };

    let atoms = AtomTable::new();
    for (name, term) in [
        ("integer list", ints),
        ("char list", chars),
        ("improper list", improper),
        ("one-cell list", single),
        ("nested list", nested),
    ] {
        // `used() == size_of_copy` is the word-count half: the chain walk writes exactly the
        // cells the per-cell path wrote, nothing dropped and nothing doubled.
        let (copy, dst) = copy_exact(term);
        assert!(
            beam_term::compare::eq(term, copy, &atoms),
            "{name}: the copy must be =:= the original"
        );
        assert_eq!(
            dst.used(),
            size_of_copy(term),
            "{name}: the chain writes exactly the source graph's words"
        );
    }
}

/// A literal-area subterm inside a chain is still **shared by identity**: it costs no
/// destination word beyond the cell that refers to it (`copy.c`'s `litopt` skip), and the copied
/// cell names the very same object.
#[test]
fn a_literal_child_inside_a_chain_costs_no_destination_word() {
    let mut src = Heap::new(32).expect("source heap");
    let literal_tuple = src.tuple(&[make_small(1), make_small(2)]).expect("the tuple fits");
    let literal = make_literal(literal_tuple);
    let term = src.cons(literal, NIL).expect("the cons fits");

    let (copy, dst) = copy_exact(term);
    assert_eq!(dst.used(), 2, "the chain writes the one cons cell only");
    assert!(
        dst.used() < size_of_copy(term),
        "the shared literal's interior is not copied"
    );
    // SAFETY: `term` and `copy` are live cons cells.
    let (src_car, dst_car) = unsafe { (car(term), car(copy)) };
    assert_eq!(dst_car.raw(), src_car.raw(), "the literal is shared, not copied");
    // The whole-cell equality still holds: sharing is not a shape change.
    assert!(beam_term::compare::eq(term, copy, &AtomTable::new()));
}

/// An off-heap child inside a chain is refcounted into the destination table **once per
/// occurrence**, exactly as the per-cell path did — the chain loop must not coalesce or skip
/// the registration that `shallow_unshared`'s boxed arm performs.
#[test]
fn an_off_heap_child_inside_a_chain_registers_once_per_occurrence() {
    let mut src = Heap::new(32).expect("source heap");
    let mut src_off = OffHeapTable::new();
    let slot = src_off.push(Mso::new(0x5A5A_u32, 12));
    // SAFETY: the heap owns these three words and the slot is the table's first entry.
    let container = unsafe {
        let p = src.alloc_words(ERL_BIN_REF_SIZE).expect("the container fits");
        *p = HEADER_BIN_REF;
        *p.add(1) = Eterm::from_raw(slot);
        *p.add(2) = Eterm::from_raw(0);
        make_boxed(p as *const Eterm)
    };
    let term = build_list(&mut src, &[container, make_small(9), container]);

    let mut dst = Heap::new(16).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src_off, &mut dst_off);
    let copy = copy_struct(term, &mut dst, &mut ctx).expect("the copy fits");
    assert_eq!(dst.used(), 6 + 2 * 3, "three cells and two 3-word containers");

    assert_eq!(
        dst_off.len(),
        2,
        "one registration per occurrence, each in its own slot"
    );
    // SAFETY: `copy` is a live 3-cell list, so each `cdr` is a live cell.
    let (first, third) = unsafe { (car(copy), car(cdr(cdr(copy)))) };
    let (first_slot, third_slot) = (layout::off_heap_slot(first), layout::off_heap_slot(third));
    assert_ne!(first_slot, third_slot, "each occurrence gets a fresh slot");
    assert_eq!(
        (first_slot.min(third_slot), first_slot.max(third_slot)),
        (0, 1),
        "the two occurrences take the destination table's own first slots"
    );
    for slot in [0usize, 1] {
        assert_eq!(
            dst_off.get(slot).map(beam_gc::MsoHandle::apparent_size),
            Some(12),
            "slot {slot} resolves to the shared payload"
        );
    }
}

// -----------------------------------------------------------------------
// The chain walk's cycle bound
// -----------------------------------------------------------------------

/// A cons cell whose `cdr` points back at itself: the forged term `[1 | <self>]`.
fn self_referential_cons(heap: &mut Heap) -> Eterm {
    let list = heap.cons(make_small(1), NIL).expect("the cons fits");
    // SAFETY: `heap.cons` just reserved this cell, which the heap exclusively owns, so
    // `list_val(list).add(1)` is its own `cdr` word. The write makes the graph cyclic on
    // purpose — exactly the malformed input the chain walk's capacity bound must survive.
    unsafe { *list_val(list).add(1) = list };
    list
}

/// A forged cyclic `cdr` chain must stay a typed `BadArg` on **both** copy routes, and the test
/// returning at all is the "the chain walk never spins" evidence.
///
/// The capacities span the two ways the bound can bite: a destination too small for even one
/// more cell (`2`, `4`) and one large enough (`4096` = 2048 cells) that only the cycle — not
/// capacity — can be the failure.
#[test]
fn a_forged_cdr_cycle_is_a_typed_badarg_on_both_copy_paths() {
    for capacity in [2usize, 4, 64, 4096] {
        // The single-pass (fast) path: the trial writes through its bump pointer, the chain
        // bound stops it, and the sized path re-derives the preflight's cycle verdict.
        let mut src = Heap::new(8).expect("source heap");
        let cyclic = self_referential_cons(&mut src);
        let mut dst = Heap::new(capacity).expect("destination heap");
        match copy_struct_simple(cyclic, &mut dst) {
            Err(BeamError::BadArg) => {}
            other => panic!("capacity {capacity}: expected BadArg, got {other:?}"),
        }
        assert_eq!(dst.used(), 0, "capacity {capacity}: a refusal writes no word");

        // The sized path (a non-empty destination off-heap table, so no trial): the preflight
        // refuses before a word is written and keeps the entry that was already there.
        let mut src = Heap::new(8).expect("source heap");
        let cyclic = self_referential_cons(&mut src);
        let src_off = OffHeapTable::new();
        let mut dst_off = OffHeapTable::new();
        dst_off.push(Mso::new(1_u32, 4));
        let mut dst = Heap::new(capacity).expect("destination heap");
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        match copy_struct(cyclic, &mut dst, &mut ctx) {
            Err(BeamError::BadArg) => {}
            other => panic!("capacity {capacity}: expected BadArg, got {other:?}"),
        }
        assert_eq!(dst.used(), 0, "capacity {capacity}: a refusal writes no word");
        assert_eq!(dst_off.len(), 1, "and the table's own entry survives");
    }
}
