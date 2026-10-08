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

//! Integration tests for the **allocation-failure classification** of the copy boundary
//! ([`is_message_heap_exhaustion`](beam_message::is_message_heap_exhaustion)).
//!
//! This is the evidence a host runtime (e.g. `beam_rt`) keys on to translate a message-heap
//! exhaustion into the emulator's documented `error:system_limit`:
//!
//! 1. a too-small destination heap fails with a **distinguishable** failure -- `AllocKind::Message`
//!    and `requested == words * size_of::<Eterm>()` -- raised by a real [`copy_struct`] /
//!    [`copy_struct_simple`] into a real [`beam_alloc::Heap`];
//! 2. the public predicate recognises exactly that failure and rejects every other `BeamError`
//!    (a different allocation class, the malformed-graph `BadArg`);
//! 3. the failure is attributed to the word-count boundary and leaves the destination heap
//!    untouched (`used() == 0`).
//!
//! The oracle for the *atom and class the host must produce* is the system `erl`: `error` /
//! `system_limit` (class `error`, reason atom `system_limit`).

use beam_alloc::Heap;
use beam_common::{AllocFailure, AllocKind, BeamError};
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, copy_struct_simple, is_message_heap_exhaustion, size_of_copy};
use beam_message::CopyContext;
use beam_term::prelude::*;

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first, synchronously (no recursion).
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the list fits the source heap");
    }
    acc
}

/// Builds an off-heap MSO reference container `[HEADER_BIN_REF, slot, 0]` on `heap`.
///
/// The container's `val` word is an MSO **slot index**; with an empty source table a copy of it
/// hits the resolution miss and yields the malformed-graph `BadArg` (`copy.c`'s contract).
fn off_heap_container(heap: &mut Heap, slot: usize) -> Eterm {
    // SAFETY: the heap owns these three words.
    unsafe {
        let p = heap.alloc_words(3).expect("the container fits");
        *p = HEADER_BIN_REF;
        *p.add(1) = Eterm::from_raw(slot);
        *p.add(2) = Eterm::from_raw(0);
        make_boxed(p as *const Eterm)
    }
}

/// The `Alloc` failure behind `error`, panicking if `error` has the wrong shape.
fn alloc_failure(error: &BeamError) -> AllocFailure {
    match error {
        BeamError::Alloc(failure) => *failure,
        other => panic!("expected an allocation failure, got {other:?}"),
    }
}

#[test]
fn too_small_heap_yields_a_distinguishable_message_exhaustion() {
    let mut src = Heap::new(128).expect("source heap");
    let list = build_list(&mut src, &[make_small(1); 50]);
    let needed_words = size_of_copy(list);
    assert_eq!(needed_words, 100, "50 cons cells are 100 words");

    let mut dst = Heap::new(4).expect("a destination heap far too small");
    let error = copy_struct_simple(list, &mut dst).expect_err("the copy cannot fit");

    // (1) The predicate recognises it ...
    assert!(
        is_message_heap_exhaustion(&error),
        "a too-small destination heap is a message-heap exhaustion"
    );
    // ... and the shape is exactly the documented contract the host keys on: kind `Message`,
    // requested bytes = words * size_of::<Eterm>().
    let failure = alloc_failure(&error);
    assert_eq!(failure.kind, AllocKind::Message);
    assert_eq!(
        failure.requested,
        needed_words * core::mem::size_of::<Eterm>()
    );

    // (3) The preflight rejects before any allocation: the destination heap is untouched.
    assert_eq!(dst.used(), 0, "the preflight rejects before any allocation");
}

#[test]
fn copy_struct_with_a_context_classifies_identically() {
    // The same contract holds on the general entry point (an explicit `CopyContext`), not only on
    // the `copy_struct_simple` convenience.
    let mut src = Heap::new(64).expect("source heap");
    let tuple = src
        .tuple(&[make_small(7), make_small(8), make_small(9)])
        .expect("the tuple fits");
    let list = build_list(&mut src, &[tuple]);
    let needed_words = size_of_copy(list);
    assert_eq!(needed_words, 6, "a cons cell + a 4-word tuple = 6 words");

    let src_off_heap = OffHeapTable::new();
    let mut dst_off_heap = OffHeapTable::new();
    let mut dst = Heap::new(4).expect("too small for the whole graph");
    let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);

    let error = copy_struct(list, &mut dst, &mut ctx).expect_err("the copy cannot fit");
    assert!(is_message_heap_exhaustion(&error));
    let failure = alloc_failure(&error);
    assert_eq!(failure.kind, AllocKind::Message);
    assert_eq!(
        failure.requested,
        needed_words * core::mem::size_of::<Eterm>()
    );
    assert_eq!(dst.used(), 0);
}

#[test]
fn exhaustion_is_exactly_the_flat_word_count_boundary() {
    let mut src = Heap::new(128).expect("source heap");
    let list = build_list(&mut src, &[make_small(1); 50]);
    let needed = size_of_copy(list);

    // A heap of exactly the flat word count fits the whole graph.
    let mut exact = Heap::new(needed).expect("an exactly-sized destination heap");
    let copy = copy_struct_simple(list, &mut exact).expect("the exact fit succeeds");
    assert_eq!(exact.used(), needed);
    // The successful copy is a real copy, not the shared source pointer.
    assert_ne!(copy.raw(), list.raw());

    // One word short is the documented exhaustion, and nothing is written.
    let mut short = Heap::new(needed - 1).expect("one word short");
    let error = copy_struct_simple(list, &mut short).expect_err("one word short must fail");
    assert!(is_message_heap_exhaustion(&error));
    let failure = alloc_failure(&error);
    assert_eq!(failure.kind, AllocKind::Message);
    assert_eq!(failure.requested, needed * core::mem::size_of::<Eterm>());
    assert_eq!(short.used(), 0);
}

#[test]
fn predicate_rejects_a_malformed_graph_badarg_from_a_real_copy() {
    // The copy engine's *other* failure -- a malformed graph (an off-heap slot no source table
    // owns) -- is `BadArg`, and the predicate must not confuse it with heap exhaustion.
    let src_off_heap = OffHeapTable::new();
    let mut src = Heap::new(8).expect("source heap");
    let container = off_heap_container(&mut src, 7);

    let mut dst_off_heap = OffHeapTable::new();
    let mut dst = Heap::new(8).expect("a destination heap with room for the container");
    let mut ctx = CopyContext::new(&src_off_heap, &mut dst_off_heap);

    let error = copy_struct(container, &mut dst, &mut ctx).expect_err("the slot has no entry");
    assert!(matches!(error, BeamError::BadArg), "got {error:?}");
    assert!(
        !is_message_heap_exhaustion(&error),
        "BadArg is not a message-heap exhaustion"
    );
    assert_eq!(dst.used(), 0);
}

#[test]
fn predicate_rejects_non_message_allocation_classes() {
    // Every other allocation class is a different signal: only `Message` means this crate's
    // copy boundary could not fit the term.
    for kind in [
        AllocKind::Heap,
        AllocKind::LargeObject,
        AllocKind::Carrier,
        AllocKind::Other,
    ] {
        let error = BeamError::Alloc(AllocFailure::new(kind, 128));
        assert!(
            !is_message_heap_exhaustion(&error),
            "{kind:?} must not classify as a message-heap exhaustion"
        );
    }

    // ... and the `Message` class built directly (through `beam_common`) does classify.
    assert!(is_message_heap_exhaustion(&BeamError::Alloc(
        AllocFailure::new(AllocKind::Message, 16)
    )));
}

#[test]
fn predicate_rejects_non_allocation_errors() {
    // The non-`Alloc` variants are never this crate's heap exhaustion.
    for error in [
        BeamError::BadArg,
        BeamError::BadArith,
        BeamError::BadArity,
        BeamError::BadMatch,
        BeamError::SystemLimit,
        BeamError::TermNotFound,
    ] {
        assert!(
            !is_message_heap_exhaustion(&error),
            "{error:?} must not classify as a message-heap exhaustion"
        );
    }
}
