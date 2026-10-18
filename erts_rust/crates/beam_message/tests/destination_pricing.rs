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

//! Adversarial pins for the copier's **destination-pricing** cost model.
//!
//! `src/copy.rs`'s `bounded_words`/`preflight_words` walk answers a *different* question from
//! `beam_term::size_object`: it prices what [`copy_struct`] will **write into the destination**,
//! not how many words the source graph occupies. The C draws exactly the same distinction —
//! `size_object_x(obj, litopt)` skips literal-area subterms only when `litopt != NULL`
//! (`erts/emulator/beam/copy.c:105,119`), `size_object(t)` is `size_object_x(t, NULL)`
//! (`global.h:1135`), and the *message-heap* charge is the `litopt`-carrying form
//! (`erl_message.c:771,839`; `global.h:1136`). So `beam_term::size_object` is the
//! `litopt == NULL` answer while `bounded_words` is the `litopt != NULL` — i.e. copier — answer.
//!
//! Two load-bearing deltas make a wholesale delegation of that walk to
//! `beam_term::size::size_object_bounded` **wrong**, and each is pinned below against the public
//! surface (`copy_struct` / `size_of_copy`) so the two cost models cannot be silently unified:
//!
//! 1. **Literal-area subterms.** `beam_term::size::needs_visit` is `is_term_word(t) &&
//!    !is_immediate(t)`, so it descends *into* literals. The copier's `needs_visit` is
//!    `!is_shared(t) && is_term_word(t)`, and `is_shared` is `is_literal(t)` for compound words —
//!    a literal is returned **by identity**, so it costs **0** destination words. Delegating would
//!    make the preflight demand words the copy never writes and refuse a copy that succeeds today.
//! 2. **A refc binary whose payload no source off-heap handle owns.** `beam_term::size::classify`
//!    prices it as the 8-word `[BinRef][ErlSubBits]` pair unconditionally. The copier **flattens**
//!    such a payload into a fresh on-heap bitstring, so its price is
//!    `heap_bits_size_words(end - start)`, typically far larger. Delegating would **under**-count,
//!    so the sized path would reserve too few words and fail *mid-copy* — breaking the "total
//!    no-op on refusal" invariant `tests/sizing.rs` and `tests/single_pass.rs` pin.
//!
//! Delta 2 is the one no term-only walk can ever reproduce: it depends on the [`CopyContext`]'s
//! **source off-heap table** (`refc_binary_words` resolves the payload's `BinRef.val` to a handle
//! there, and `copy_refc_binary` flattens when that resolution fails). `beam_term` has no such
//! table and no such parameter. See `src/copy.rs`'s module docs for the standing rationale.
//!
//! `tests/sizing.rs` remains the *agreement* contract for the corpus where the two counts coincide
//! (`size_of_copy(term) == size_object(term)` and the copy writes exactly that); this file is the
//! complementary *divergence* contract for the shapes where they must not.

use beam_alloc::{Heap, RefcBinary};
use beam_gc::Mso;
use beam_message::off_heap::OffHeapTable;
use beam_message::{
    copy_struct, is_message_heap_exhaustion, size_of_copy, CopyContext,
};
use beam_term::prelude::*;
use beam_term::size_object;

/// A destination for one copy: a heap of `capacity_words` with its first `precharged` words
/// already handed out (so `available()` is `capacity_words - precharged`), plus an off-heap table
/// that either starts **empty** — the copier's single-pass fast path — or holds a decoy MSO handle,
/// which makes the table non-empty and therefore forces the **sized** (preflight-then-write) path.
///
/// Every pin below runs against both, so each one constrains the *pricing* rather than one of the
/// two routes that consult it.
fn destination(capacity_words: usize, precharged: usize, decoy: bool) -> (Heap, OffHeapTable) {
    let mut heap = Heap::new(capacity_words).expect("destination heap");
    if precharged > 0 {
        // SAFETY: the heap owns these words and the test never reads them.
        unsafe { heap.alloc_words(precharged).expect("the pre-charge fits") };
    }
    let mut off = OffHeapTable::new();
    if decoy {
        // A pre-existing off-heap entry takes the sized path (`is_fast_path_eligible` is false).
        off.push(Mso::new(0xDEC0_0000_u32, 1));
    }
    (heap, off)
}

/// A 200-byte payload whose bytes are position-dependent, so a partial or shifted copy is visible.
fn payload() -> Vec<u8> {
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

// --- delta 1: literal-area subterms cost 0 destination words ------------------------------

/// Delta 1 at the root: a **literal** compound is shared by identity, so it costs **0** destination
/// words — even though `size_object` (the `litopt == NULL` count) reports its whole interior.
///
/// A destination with **no spare word at all** therefore still accepts the copy. Delegating the
/// preflight to `beam_term::size_object` would demand `size_object(term)` words here and refuse it.
#[test]
fn a_literal_root_costs_zero_destination_words_although_size_object_counts_its_interior() {
    let mut src = Heap::new(16).expect("source heap");
    let tuple = src.tuple(&[make_small(1), make_small(2)]).expect("the tuple fits");
    let literal = make_literal(tuple);

    // `beam_term::size_object` = `size_object_x(t, NULL)`: the walk descends INTO the literal.
    assert_eq!(
        size_object(literal),
        3,
        "the source-sizing walk counts the literal's 1-word header + 2 elements"
    );
    assert_eq!(
        size_of_copy(literal),
        3,
        "`size_of_copy` is the `size_object` forwarder, so it reports the same source count"
    );

    for decoy in [false, true] {
        // Capacity 3, all of it already handed out: `available() == 0`. Only a copier that prices
        // a shared literal at 0 words can accept this copy.
        let (mut dst, mut dst_off) = destination(3, 3, decoy);
        assert_eq!(dst.available(), 0, "the destination has no spare word");
        let before = dst.used();
        let copy = {
            let src_off = OffHeapTable::new();
            let mut ctx = CopyContext::new(&src_off, &mut dst_off);
            copy_struct(literal, &mut dst, &mut ctx).unwrap_or_else(|e| {
                panic!("(decoy={decoy}) a shared literal needs no destination word, got {e:?}")
            })
        };
        assert_eq!(copy.raw(), literal.raw(), "(decoy={decoy}) a literal is shared by identity");
        assert_eq!(
            dst.used(),
            before,
            "(decoy={decoy}) the copy contributes exactly 0 destination words"
        );
        assert_eq!(
            dst.used(),
            dst.capacity(),
            "(decoy={decoy}) the pre-charge is the whole occupancy — nothing was written"
        );
        assert_eq!(
            dst.peak_used(),
            before,
            "(decoy={decoy}) not even transiently: the trial never advanced the heap"
        );
    }
}

/// Delta 1 nested: a literal **child** of a copied object is still 0 destination words, so the copy
/// writes only the outer object — while `size_object` counts the outer object *and* the literal's
/// interior. This is the shape that makes the delta a general pricing rule rather than a root case.
#[test]
fn a_literal_child_costs_zero_destination_words_although_size_object_counts_its_interior() {
    let mut src = Heap::new(16).expect("source heap");
    let inner = src
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("the inner tuple fits");
    let literal = make_literal(inner);
    let root = src.tuple(&[literal, make_small(9)]).expect("the outer tuple fits");

    // The source count: the 3-word outer tuple + the 4-word literal interior.
    assert_eq!(size_object(root), 7, "the source-sizing walk descends into the literal child");
    assert_eq!(size_of_copy(root), 7, "`size_of_copy` is that same source count");

    for decoy in [false, true] {
        // Exactly the copier's own price (the 3-word outer tuple) and not one word more.
        let (mut dst, mut dst_off) = destination(3, 0, decoy);
        let copy = {
            let src_off = OffHeapTable::new();
            let mut ctx = CopyContext::new(&src_off, &mut dst_off);
            copy_struct(root, &mut dst, &mut ctx).unwrap_or_else(|e| {
                panic!("(decoy={decoy}) the copier prices only the outer tuple, got {e:?}")
            })
        };
        assert_eq!(
            dst.used(),
            3,
            "(decoy={decoy}) the copy writes the outer tuple only"
        );
        assert_ne!(
            size_of_copy(root),
            dst.used(),
            "(decoy={decoy}) the destination price is NOT `size_object` for this shape"
        );
        // SAFETY: `copy` is a live 2-tuple and slot 0 is one of its element words.
        let child = unsafe { tuple_element(copy, 0) };
        assert_eq!(child.raw(), literal.raw(), "(decoy={decoy}) the child is the shared literal");
        assert!(is_literal(child), "(decoy={decoy}) not a fresh copy of the literal");
        assert!(
            !dst_off.binaries().is_empty() || !decoy || dst_off.len() == 1,
            "(decoy={decoy}) no unexpected off-heap artifact"
        );
    }
}

// --- delta 2: a no-handle refc binary costs its flattened window --------------------------

/// Delta 2: a refc binary whose payload **no source off-heap handle owns** is flattened into a
/// fresh on-heap bitstring, so the copier's price is the flattened block — strictly **more** than
/// the 8-word `[BinRef][ErlSubBits]` pair `size_object` reports.
///
/// A destination sized exactly to `size_of_copy` (i.e. to what a delegation to `beam_term`'s walk
/// would preflight) is therefore **refused**, totally, and one sized to the flattened window
/// succeeds. An under-count here would under-reserve the destination and fail *mid-copy*, leaving a
/// partial write.
#[test]
fn a_no_handle_refc_binary_costs_the_flattened_window_and_a_size_object_destination_is_refused() {
    let bytes = payload();
    let bits = bytes.len() * 8;
    let flat = heap_bits_size_words(bits);
    assert!(flat > ERL_REFC_BITS_SIZE, "the flattened block is the larger of the two");

    let source_binary = RefcBinary::new(&bytes).expect("the payload allocates");
    let data_ptr = source_binary.data_ptr();
    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: `source_binary` outlives `pair` and `data_ptr` is `UNIT_SIZE`-aligned.
    let pair = unsafe { refc_binary_term(&mut src, data_ptr, bytes.len()) };
    assert!(unsafe { is_refc_binary(pair) });

    // The source count, which cannot see the missing handle.
    assert_eq!(
        size_object(pair),
        ERL_REFC_BITS_SIZE,
        "beam_term prices the 8-word pair unconditionally"
    );
    assert_eq!(size_of_copy(pair), ERL_REFC_BITS_SIZE, "`size_of_copy` reports that same count");

    // The source off-heap table does NOT hold the handle: there is no pair to share.
    let src_off = OffHeapTable::new();
    for decoy in [false, true] {
        // Sized to `size_object` — a delegation's preflight would pass this — yet refused.
        let (mut tight, mut tight_off) = destination(ERL_REFC_BITS_SIZE, 0, decoy);
        let error = {
            let mut ctx = CopyContext::new(&src_off, &mut tight_off);
            copy_struct(pair, &mut tight, &mut ctx)
                .expect_err("the flattened window does not fit a `size_object`-sized destination")
        };
        assert!(
            is_message_heap_exhaustion(&error),
            "(decoy={decoy}) a refused flatten is a message-heap exhaustion, got {error:?}"
        );
        assert_eq!(tight.used(), 0, "(decoy={decoy}) the refusal writes no destination word");
        assert_eq!(tight.peak_used(), 0, "(decoy={decoy}) nothing was even temporarily written");
        assert!(
            tight_off.binaries().is_empty(),
            "(decoy={decoy}) and no refc-binary handle is registered"
        );

        // Sized to the flattened window: accepted, and the copy costs exactly that.
        let (mut exact, mut exact_off) = destination(flat, 0, decoy);
        let copy = {
            let mut ctx = CopyContext::new(&src_off, &mut exact_off);
            copy_struct(pair, &mut exact, &mut ctx).unwrap_or_else(|e| {
                panic!("(decoy={decoy}) exactly the flattened window must fit, got {e:?}")
            })
        };
        assert_eq!(
            exact.used(),
            flat,
            "(decoy={decoy}) the copier's price is the flattened block, not the 8-word pair"
        );
        assert_ne!(
            size_of_copy(pair),
            exact.used(),
            "(decoy={decoy}) the destination price is NOT `size_object` for this shape"
        );
        assert!(
            !unsafe { is_refc_binary(copy) },
            "(decoy={decoy}) the copy is a flattened on-heap bitstring"
        );
        // SAFETY: `copy` is a live on-heap bitstring covering the whole window.
        assert_eq!(unsafe { bitstring_parts(copy) }.2, bits, "(decoy={decoy}) the whole window");
        assert!(
            exact_off.binaries().is_empty(),
            "(decoy={decoy}) the flatten registers nothing in the off-heap table"
        );
    }
}

/// The contrast that proves delta 2 is about the **missing handle**, not the term shape: the *same*
/// refc binary, with its payload owned by a handle in the source table, costs exactly the shared
/// 8-word pair — i.e. exactly `size_object`. Only a walk parameterized by the copy boundary's source
/// off-heap table can tell the two apart, which is why this pricing cannot live in `beam_term`.
#[test]
fn a_handled_refc_binary_costs_exactly_the_shared_pair_so_the_delta_is_context_not_shape() {
    let bytes = payload();
    let source_binary = RefcBinary::new(&bytes).expect("the payload allocates");
    let data_ptr = source_binary.data_ptr();
    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: `source_binary` is moved into `src_off`, which outlives `pair`, and `data_ptr` is
    // `UNIT_SIZE`-aligned.
    let pair = unsafe { refc_binary_term(&mut src, data_ptr, bytes.len()) };
    let mut src_off = OffHeapTable::new();
    src_off.push_binary(source_binary);

    assert_eq!(size_object(pair), ERL_REFC_BITS_SIZE, "the source count is unchanged");
    assert_eq!(size_of_copy(pair), ERL_REFC_BITS_SIZE);

    for decoy in [false, true] {
        // Exactly the 8-word pair: the two cost models AGREE here, which is what makes the delta 2
        // pin above a genuine semantic difference rather than a different naming of the same count.
        let (mut dst, mut dst_off) = destination(ERL_REFC_BITS_SIZE, 0, decoy);
        let copy = {
            let mut ctx = CopyContext::new(&src_off, &mut dst_off);
            copy_struct(pair, &mut dst, &mut ctx)
                .unwrap_or_else(|e| panic!("(decoy={decoy}) the 8-word pair must fit, got {e:?}"))
        };
        assert_eq!(
            dst.used(),
            size_of_copy(pair),
            "(decoy={decoy}) a handled refc binary costs exactly `size_object`"
        );
        assert_eq!(dst_off.binaries().len(), 1, "(decoy={decoy}) the payload handle is shared");
        assert_eq!(
            dst_off.binaries()[0].data_ptr(),
            data_ptr,
            "(decoy={decoy}) the payload itself is not deep-copied"
        );
        assert!(unsafe { is_refc_binary(copy) }, "(decoy={decoy}) still a refc binary");
    }
}

/// The decisive consequence of an **under**-counted gate: a graph whose no-handle refc binary is
/// priced as the 8-word pair instead of the flattened window passes the preflight with room to
/// spare, the copy starts writing the *other* objects, and only the flatten fails — a **partial
/// write**. That is the exact invariant the crate's "total no-op on refusal" rule forbids, and it
/// can only be caught on the sized path (a non-empty destination off-heap table), where the
/// preflight *is* the gate and `shallow` writes straight into the destination heap.
///
/// A destination one word short of the copier's true price is refused with not a single word
/// written, while a gate built on `size_object` would have found `size_object(root)` words plenty.
#[test]
fn the_preflight_gates_the_whole_graph_so_an_under_priced_flatten_cannot_partially_write() {
    let bytes = payload();
    let bits = bytes.len() * 8;
    let flat = heap_bits_size_words(bits);
    assert!(flat > ERL_REFC_BITS_SIZE, "the flattened block is the larger of the two");

    let source_binary = RefcBinary::new(&bytes).expect("the payload allocates");
    let data_ptr = source_binary.data_ptr();
    let mut src = Heap::new(256).expect("source heap");
    // SAFETY: `source_binary` outlives `pair` and `data_ptr` is `UNIT_SIZE`-aligned.
    let pair = unsafe { refc_binary_term(&mut src, data_ptr, bytes.len()) };
    // A 50-cell list behind the refc binary, so the copy has real work to do before the flatten.
    let mut list = NIL;
    for i in 0..50i64 {
        list = src.cons(make_small(i), list).expect("the cons fits");
    }
    let root = src.tuple(&[pair, list]).expect("the root fits");

    let list_words = 2 * 50;
    let true_price = 3 + flat + list_words;
    let source_price = 3 + ERL_REFC_BITS_SIZE + list_words;
    assert_eq!(size_object(root), source_price, "the source count prices the 8-word pair");
    assert!(true_price > source_price, "and so under-counts the destination by the flatten delta");

    // A destination one word short of the copier's true price, with a decoy off-heap entry so the
    // sized path (preflight-then-write) is the route under test.
    let (mut dst, mut dst_off) = destination(true_price - 1, 0, true);
    assert_eq!(dst.available(), true_price - 1);
    let error = {
        let src_off = OffHeapTable::new();
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        copy_struct(root, &mut dst, &mut ctx)
            .expect_err("one word short of the copier's price must be refused")
    };
    assert!(
        is_message_heap_exhaustion(&error),
        "the refusal is a message-heap exhaustion, got {error:?}"
    );
    assert_eq!(
        dst.used(),
        0,
        "the gate admits only the whole graph, so the refusal writes no destination word"
    );
    assert_eq!(dst.peak_used(), 0, "nothing was even temporarily written");
    assert!(dst_off.binaries().is_empty(), "and no refc-binary handle was registered");

    // At exactly the copier's price the same graph is accepted, and it costs exactly that.
    let (mut ok, mut ok_off) = destination(true_price, 0, true);
    {
        let src_off = OffHeapTable::new();
        let mut ctx = CopyContext::new(&src_off, &mut ok_off);
        copy_struct(root, &mut ok, &mut ctx)
            .expect("exactly the copier's price must be accepted")
    };
    assert_eq!(ok.used(), true_price, "the copy writes the flat window, not the 8-word pair");
    assert_ne!(
        size_of_copy(root),
        ok.used(),
        "and the destination price is NOT `size_object` for this graph"
    );
}
