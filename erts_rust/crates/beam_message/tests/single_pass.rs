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

//! The **single-pass fast path**: the observable contract of the clean-destination copy.
//!
//! `copy_struct` serves a *clean* destination (`Heap::used() == 0` and an off-heap table empty in
//! all three of its lists) with ONE traversal - the writing traversal, with no sizing walk ahead of
//! it - and keeps the preflight-then-write path for every other destination. That split is an
//! internal optimisation, so this file pins the properties a caller can see, and every one of them
//! must hold for **both** paths:
//!
//! 1. **A clean copy is a faithful copy.** For a corpus spanning a small tuple, a cons chain, a
//!    flat map, a boxed float, an off-heap MSO container and a refc binary, a clean-destination
//!    copy is structurally equal to the original and writes exactly `size_of_copy(term)` words -
//!    the same result, and the same word count, as the copy of the same graph into a destination
//!    that is deliberately **dirty** (pre-charged with one word, hence always preflighted).
//! 2. **A refusal is still a total no-op.** A clean destination one word short refuses with the
//!    typed message-heap allocation failure and leaves `used() == peak_used() == 0`: the
//!    optimistic attempt writes through a trial bump pointer that only commits on success, so a
//!    refused copy has written nothing at all - not even temporarily.
//! 3. **A refusal releases what it registered.** An attempt that refcounted off-heap objects
//!    before running out of room gives those references back: the destination off-heap table ends
//!    up empty and the source objects' refcounts are unchanged.
//! 4. **A forged cycle is still a typed error.** The fast path inherits the cycle guarantee from
//!    the other direction: its trial target hands out only the destination's finite free words, so
//!    a cycle exhausts them, the attempt is abandoned and the preflight path's acyclicity check
//!    re-derives the typed `BadArg`. The test *completing* is the "never hangs" evidence.
//! 5. **A dirty destination is never rolled back.** A destination whose heap or off-heap table
//!    already holds something keeps the sized path, and nothing that was already there is
//!    disturbed - neither the heap's pre-charged word nor a pre-existing off-heap entry.
//!
//! Two documented `beam_term::compare` limitations shape the assertions: it has no structural arm
//! for an off-heap MSO container (whose `val` is a slot index rather than a term), so that shape's
//! equality is asserted through the container accessors, exactly as `tests/guards.rs` does.

use beam_alloc::{Heap, RefcBinary};
use beam_common::{AllocKind, BeamError};
use beam_gc::Mso;
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, size_of_copy, CopyContext};
use beam_term::compare;
use beam_term::prelude::*;

/// A source heap with the atom table and off-heap table the shapes may need.
struct Src {
    heap: Heap,
    atoms: AtomTable,
    off: OffHeapTable,
}

impl Src {
    fn new() -> Src {
        Src {
            heap: Heap::new(1 << 16).expect("source heap"),
            atoms: AtomTable::new(),
            off: OffHeapTable::new(),
        }
    }
}

/// How a shape's copy is compared with its original.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Equal {
    /// `beam_term::compare::eq` (the structural equality of every class it models).
    Structural,
    /// The off-heap container accessors (`beam_term::compare` has no structural arm for it).
    MsoContainer,
    /// A 2-tuple of an off-heap MSO container and a refc binary: the pair is compared element by
    /// element through the container accessors and the binary's bytes.
    TupleWithOffHeap,
}

/// Builds a 3-element tuple (the smallest realistic message payload).
fn small_tuple(s: &mut Src) -> Eterm {
    s.heap
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("the tuple fits")
}

/// Builds a 50-cell cons chain.
fn deep_list(s: &mut Src) -> Eterm {
    let mut acc = NIL;
    for i in 0..50 {
        acc = s.heap.cons(make_small(i % 97), acc).expect("the cons fits");
    }
    acc
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

/// Builds an 8-key flat map.
fn flat_map(s: &mut Src) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (0..8)
        .map(|i| (make_small(i), make_small(i * 10 + 1)))
        .collect();
    write_map_term(&mut s.heap, &pairs, &s.atoms)
}

/// Builds a boxed float (`[HEADER_FLONUM, bits]`, 2 words).
fn boxed_float(s: &mut Src) -> Eterm {
    // SAFETY: two words are reserved for the `[HEADER_FLONUM, bits]` block.
    unsafe {
        let hp = s.heap.alloc_words(2).expect("the float block fits");
        write_float(hp, core::f64::consts::PI)
    }
}

/// Builds an off-heap MSO reference container `[HEADER_BIN_REF, slot, 0]` on `s.heap`, whose slot
/// is the first entry of `s.off`.
fn off_heap_container(s: &mut Src) -> Eterm {
    let slot = s.off.push(Mso::new(0x5A5A_u32, 12));
    assert_eq!(slot, 0, "the container's slot is the only off-heap object");
    // SAFETY: the heap owns these three words.
    unsafe {
        let p = s.heap.alloc_words(ERL_BIN_REF_SIZE).expect("the container fits");
        *p = HEADER_BIN_REF;
        *p.add(1) = Eterm::from_raw(slot);
        *p.add(2) = Eterm::from_raw(0);
        make_boxed(p as *const Eterm)
    }
}

/// Builds a refc binary (the 8-word `[BinRef][ErlSubBits]` pair) on `s.heap`, its payload owned by
/// `s.off`.
fn refc_binary(s: &mut Src) -> Eterm {
    let bytes: Vec<u8> = (0..200usize).map(|i| ((i * 7) % 251) as u8).collect();
    s.off
        .push_binary(RefcBinary::new(&bytes).expect("the payload allocates"));
    let data_ptr = s.off.binaries()[0].data_ptr();
    // SAFETY: 8 words are reserved for the pair; `s.off` owns the payload, which outlives it.
    unsafe {
        let hp = s.heap.alloc_words(ERL_REFC_BITS_SIZE).expect("the pair fits");
        write_refc_binary(
            hp,
            data_ptr as *mut Eterm,
            core::ptr::null_mut(),
            data_ptr,
            0,
            bytes.len() * 8,
        )
    }
}

/// Builds a 2-tuple holding an off-heap MSO container and a refc binary: a payload that registers
/// **two** off-heap artifacts on the way down (14 words: 3 + 3 + 8).
fn container_and_binary(s: &mut Src) -> Eterm {
    let container = off_heap_container(s);
    let pair = refc_binary(s);
    s.heap
        .tuple(&[container, pair])
        .expect("the root tuple fits")
}

/// The corpus: a name, a builder and how the copy is compared with the original.
const CORPUS: &[(&str, fn(&mut Src) -> Eterm, Equal)] = &[
    ("small tuple", small_tuple, Equal::Structural),
    ("deep list (50 cons cells)", deep_list, Equal::Structural),
    ("flat map (8 keys)", flat_map, Equal::Structural),
    ("boxed float", boxed_float, Equal::Structural),
    ("refc binary", refc_binary, Equal::Structural),
    ("off-heap MSO container", off_heap_container, Equal::MsoContainer),
    (
        "container and refc binary",
        container_and_binary,
        Equal::TupleWithOffHeap,
    ),
];

/// Reads element `index` of a live tuple term.
fn tuple_element(term: Eterm, index: usize) -> Eterm {
    // SAFETY: `term` is a live tuple (an arity-value header followed by its elements), so word
    // `index + 1` is one of its elements.
    unsafe { *boxed_val(term).add(index + 1) }
}

/// Asserts that `copy` is a **relocated** off-heap `BinRef` container whose slot is the
/// destination table's first entry, matching the source container `src`.
fn assert_container_relocated(name: &str, src: Eterm, copy: Eterm) {
    // SAFETY: `src`/`copy` are live `BinRef` containers, so their header and `val` words are
    // readable.
    let (src_header, src_slot) = unsafe { (*boxed_val(src), (*boxed_val(src).add(1)).raw()) };
    let (copy_header, copy_slot) = unsafe { (*boxed_val(copy), (*boxed_val(copy).add(1)).raw()) };
    assert_eq!(src_header.raw(), HEADER_BIN_REF.raw(), "{name}: the source is a container");
    assert_eq!(copy_header.raw(), HEADER_BIN_REF.raw(), "{name}: the copy is a container");
    assert_eq!(src_slot, 0, "{name}: the source slot");
    assert_eq!(copy_slot, 0, "{name}: the copy's slot is the destination's first entry");
    assert_ne!(boxed_val(src), boxed_val(copy), "{name}: the container is relocated");
}

/// Asserts that `copy` is a faithful copy of `src`, by whichever comparison `equal` names.
fn assert_faithful(name: &str, equal: Equal, src: Eterm, copy: Eterm, atoms: &AtomTable) {
    match equal {
        Equal::Structural => assert!(compare::eq(src, copy, atoms), "{name}: the copy is =:= the original"),
        Equal::MsoContainer => {
            // The copy is a fresh container whose slot is the destination table's first entry (slot
            // 0) - the slot word is not a term, so nothing structural compares it.
            assert_container_relocated(name, src, copy);
        }
        Equal::TupleWithOffHeap => {
            // SAFETY: both terms are live 2-tuples of a `BinRef` container and a refc binary, so
            // their headers and element words are readable.
            let (src_arity, copy_arity) =
                unsafe { ((*boxed_val(src)).raw(), (*boxed_val(copy)).raw()) };
            assert_eq!(src_arity, make_arityval(2).raw(), "{name}: the original is a 2-tuple");
            assert_eq!(copy_arity, make_arityval(2).raw(), "{name}: the copy is a 2-tuple");
            assert_container_relocated(name, tuple_element(src, 0), tuple_element(copy, 0));
            let (src_binary, copy_binary) = (tuple_element(src, 1), tuple_element(copy, 1));
            // SAFETY: the second elements are live refc binaries whose payload outlives them.
            unsafe {
                assert!(
                    is_refc_binary(src_binary),
                    "{name}: the original's second element is a refc binary"
                );
                assert!(
                    is_refc_binary(copy_binary),
                    "{name}: the copy's second element is a refc binary"
                );
                assert_eq!(
                    refc_bytes(copy_binary),
                    refc_bytes(src_binary),
                    "{name}: the payload is shared, byte for byte"
                );
            }
        }
    }
}

/// Copies `term` into a **clean** destination of `capacity` words (nothing pre-charged), returning
/// the copy, the destination heap and its off-heap table.
fn copy_clean(
    term: Eterm,
    src: &Src,
    capacity: usize,
) -> Result<(Eterm, Heap, OffHeapTable), BeamError> {
    let mut dst = Heap::new(capacity).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src.off, &mut dst_off);
        copy_struct(term, &mut dst, &mut ctx)?
    };
    Ok((copy, dst, dst_off))
}

// ---------------------------------------------------------------------------
// 1. A clean destination copies every shape like the preflighted path
// ---------------------------------------------------------------------------

#[test]
fn a_clean_destination_copies_every_shape_like_the_preflighted_path() {
    for (name, build, equal) in CORPUS {
        let mut src = Src::new();
        let term = build(&mut src);
        let sized = size_of_copy(term);
        assert!(sized > 0, "{name}: every corpus shape is a compound term");

        // (a) The clean destination: `used() == 0` and an empty off-heap table, so this copy is
        // the single-pass path.
        let (fast, clean, clean_off) = copy_clean(term, &src, sized + 8)
            .unwrap_or_else(|e| panic!("{name}: a clean copy must succeed, got {e:?}"));
        assert_eq!(clean.used(), sized, "{name}: the fast copy writes the sized count");
        assert_faithful(name, *equal, term, fast, &src.atoms);

        // (b) The same graph into a destination pre-charged with one word: dirty, so this copy is
        // the preflight path. Both must agree, word count included.
        let mut dirty = Heap::new(sized + 8 + 1).expect("destination heap");
        // SAFETY: one word of this heap, reserved as scratch so `used() > 0`.
        unsafe { dirty.alloc_words(1).expect("the pre-charge fits") };
        let mut dirty_off = OffHeapTable::new();
        let slow = {
            let mut ctx = CopyContext::new(&src.off, &mut dirty_off);
            copy_struct(term, &mut dirty, &mut ctx)
                .unwrap_or_else(|e| panic!("{name}: the preflighted copy must succeed, got {e:?}"))
        };
        assert_eq!(
            dirty.used(),
            sized + 1,
            "{name}: the preflighted copy writes the sized count after the pre-charge"
        );
        assert_faithful(name, *equal, term, slow, &src.atoms);

        // The two paths register the same off-heap artifacts (same count in each category).
        assert_eq!(clean_off.len(), dirty_off.len(), "{name}: MSO handles");
        assert_eq!(
            clean_off.binaries().len(),
            dirty_off.binaries().len(),
            "{name}: refc binaries"
        );
        assert_eq!(clean_off.nodes().len(), dirty_off.nodes().len(), "{name}: nodes");
    }
}

// ---------------------------------------------------------------------------
// 2. A clean destination one word short refuses, writing nothing at all
// ---------------------------------------------------------------------------

#[test]
fn a_clean_one_word_short_destination_refuses_as_a_total_no_op() {
    for (name, build, _) in CORPUS {
        let mut src = Src::new();
        let term = build(&mut src);
        let sized = size_of_copy(term);
        assert!(sized > 1, "{name}: needs more than one word");

        let mut dst = Heap::new(sized - 1).expect("destination heap");
        let mut dst_off = OffHeapTable::new();
        match {
            let mut ctx = CopyContext::new(&src.off, &mut dst_off);
            copy_struct(term, &mut dst, &mut ctx)
        } {
            Err(BeamError::Alloc(failure)) => assert_eq!(
                failure.kind,
                AllocKind::Message,
                "{name}: exhaustion is attributed to the message heap"
            ),
            other => panic!("{name}: expected a typed allocation refusal, got {other:?}"),
        }

        assert_eq!(dst.used(), 0, "{name}: a refused copy writes no destination word");
        assert_eq!(
            dst.peak_used(),
            0,
            "{name}: the optimistic attempt wrote nothing, not even temporarily"
        );
        assert!(
            dst_off.is_empty() && dst_off.binaries().is_empty() && dst_off.nodes().is_empty(),
            "{name}: a refused copy registers no off-heap artifact"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. A refusal releases the off-heap references it registered
// ---------------------------------------------------------------------------

#[test]
fn a_refused_clean_copy_gives_back_the_references_it_registered() {
    let mut src = Src::new();
    let term = container_and_binary(&mut src);
    let sized = size_of_copy(term);

    // The refcounts are read **through the source table** rather than through a cloned handle:
    // cloning an `MsoHandle`/`RefcBinary` is itself a reference, which would inflate the count the
    // assertions below are about.
    fn mso_refcount(src: &Src) -> Option<usize> {
        src.off.get(0).map(|handle| handle.refcount())
    }
    fn binary_refcount(src: &Src) -> Option<usize> {
        src.off.binaries().first().map(RefcBinary::refcount)
    }

    assert_eq!(mso_refcount(&src), Some(1), "the source table holds the only reference");
    assert_eq!(binary_refcount(&src), Some(1), "the source table holds the only reference");

    // Sanity: a destination with room really does register both artifacts, so the roll-back below
    // is exercised on a state that the attempt actually built.
    {
        let (_, dst, dst_off) = copy_clean(term, &src, sized).expect("the copy fits exactly");
        assert_eq!(dst.used(), sized);
        assert_eq!(dst_off.len(), 1, "the MSO container is refcounted into the destination");
        assert_eq!(dst_off.binaries().len(), 1, "the refc binary is refcounted into the destination");
        assert_eq!(dst_off.nodes().len(), 0, "an MSO container and a refc binary hold no node");
        assert_eq!(mso_refcount(&src), Some(2), "source + the destination table");
        assert_eq!(binary_refcount(&src), Some(2), "source + the destination table");
    }
    assert_eq!(mso_refcount(&src), Some(1), "dropping the destination gives its reference back");
    assert_eq!(binary_refcount(&src), Some(1), "dropping the destination gives its reference back");

    // Now a clean destination one word short: the attempt registers both objects and then runs out
    // of room, so it must release them again.
    let mut short = Heap::new(sized - 1).expect("destination heap");
    let mut short_off = OffHeapTable::new();
    match {
        let mut ctx = CopyContext::new(&src.off, &mut short_off);
        copy_struct(term, &mut short, &mut ctx)
    } {
        Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Message),
        other => panic!("expected a typed allocation refusal, got {other:?}"),
    }

    assert_eq!(short.used(), 0, "the refused copy wrote nothing");
    assert_eq!(short.peak_used(), 0, "not even temporarily");
    assert!(
        short_off.is_empty() && short_off.binaries().is_empty() && short_off.nodes().is_empty(),
        "the roll-back emptied the destination's off-heap table"
    );
    assert_eq!(mso_refcount(&src), Some(1), "the MSO reference the attempt took was released");
    assert_eq!(
        binary_refcount(&src),
        Some(1),
        "the refc-binary reference the attempt took was released"
    );
}

// ---------------------------------------------------------------------------
// 4. A forged cycle on a clean destination is still a typed error
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

/// Asserts that copying the cyclic `term` into a **clean** destination of `capacity` words fails
/// with a typed `BadArg`, writing nothing and registering nothing. The test returning at all is the
/// "never hangs" evidence: the optimistic attempt exhausts the destination's finite free words and
/// the preflight path then proves the graph cyclic.
fn assert_cycle_is_refused(term: Eterm, capacity: usize) {
    let src = Src::new();
    let mut dst = Heap::new(capacity).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    match {
        let mut ctx = CopyContext::new(&src.off, &mut dst_off);
        copy_struct(term, &mut dst, &mut ctx)
    } {
        Err(BeamError::BadArg) => {}
        other => panic!("expected BadArg for a forged cycle, got {other:?}"),
    }
    assert_eq!(dst.used(), 0, "a refused copy writes no destination word");
    assert_eq!(dst.peak_used(), 0, "not even temporarily");
    assert!(dst_off.is_empty(), "a refused copy registers no off-heap artifact");
}

#[test]
fn a_forged_cycle_on_a_clean_destination_is_refused() {
    for capacity in [4usize, 4096] {
        let mut src = Src::new();
        assert_cycle_is_refused(self_referential_cons(&mut src.heap), capacity);
    }
    let mut src = Src::new();
    assert_cycle_is_refused(self_referential_tuple(&mut src.heap), 4096);
}

#[test]
fn a_non_term_on_a_clean_destination_is_refused() {
    let src = Src::new();
    let mut dst = Heap::new(64).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    match {
        let mut ctx = CopyContext::new(&src.off, &mut dst_off);
        copy_struct(THE_NON_VALUE, &mut dst, &mut ctx)
    } {
        Err(BeamError::BadArg) => {}
        other => panic!("expected BadArg for a bare header word, got {other:?}"),
    }
    assert_eq!(dst.used(), 0);
    assert_eq!(dst.peak_used(), 0);
}

// ---------------------------------------------------------------------------
// 5. A dirty destination keeps the sized path and is never rolled back
// ---------------------------------------------------------------------------

#[test]
fn a_dirty_destination_still_copies_through_the_sized_path() {
    let mut src = Src::new();
    let term = deep_list(&mut src);
    let sized = size_of_copy(term);

    let mut dst = Heap::new(sized + 16).expect("destination heap");
    // SAFETY: one word of this heap; the sentinel is written and read back below to prove the copy
    // left the words that were already there alone.
    let scratch = unsafe { dst.alloc_words(1).expect("the pre-charge fits") };
    // SAFETY: `scratch` is the single word just reserved.
    unsafe { *scratch = make_small(0x1234) };

    let mut dst_off = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src.off, &mut dst_off);
        copy_struct(term, &mut dst, &mut ctx).expect("the copy succeeds")
    };

    assert!(compare::eq(term, copy, &src.atoms), "the dirty-destination copy is correct");
    assert_eq!(dst.used(), sized + 1, "the copy lands after the pre-charged word");
    // SAFETY: `scratch` is still inside the heap's region (the heap never reallocates).
    assert_eq!(
        unsafe { *scratch }.raw(),
        make_small(0x1234).raw(),
        "the words that were already in the destination are untouched"
    );
}

#[test]
fn a_dirty_off_heap_table_is_never_rolled_back() {
    let mut src = Src::new();
    let term = small_tuple(&mut src);
    let sized = size_of_copy(term);

    // A destination whose off-heap table already holds an object: not clean, so the fast path must
    // not run - its roll-back drains the whole table and would discard this entry.
    let mut dst = Heap::new(sized - 1).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let held = Mso::new(0x1234_u32, 8);
    assert_eq!(dst_off.push(held), 0);

    match {
        let mut ctx = CopyContext::new(&src.off, &mut dst_off);
        copy_struct(term, &mut dst, &mut ctx)
    } {
        Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Message),
        other => panic!("expected a typed allocation refusal, got {other:?}"),
    }

    assert_eq!(dst.used(), 0);
    assert_eq!(dst_off.len(), 1, "the pre-existing off-heap entry survives");
    assert_eq!(
        dst_off.get(0).map(|handle| handle.refcount()),
        Some(1),
        "the pre-existing entry still holds exactly its own reference"
    );
    assert_eq!(
        dst_off.get(0).and_then(|handle| handle.downcast_ref::<u32>()),
        Some(&0x1234)
    );
}
