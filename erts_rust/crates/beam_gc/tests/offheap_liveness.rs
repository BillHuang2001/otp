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

//! **Off-heap liveness through a *moved* heap object** — the hard reclamation case.
//!
//! An off-heap object (a refcounted binary or a generic MSO) is referenced only from a
//! small on-heap *handle*. The collector copies that handle like any other term and leaves
//! the off-heap pointer **byte-identical** (the `erl_gc.c::sweep_off_heap` rule): it never
//! follows the payload, never moves it and never touches its reference count. Reclaiming
//! the payload is therefore the owning process's job — after a collection it walks its
//! rootset, decides which registered [`OffHeap`] entries a reachable handle still names, and
//! calls [`OffHeap::sweep`] with that predicate.
//!
//! `tests/gc_pressure_offheap.rs` pins an off-heap binary whose *direct* root becomes garbage
//! and one held by a moved fun environment; `tests/collector_adversarial.rs` pins an off-heap
//! binary behind a direct root and an MSO referenced by a slot index. This file closes the
//! remaining gap explicitly: the handle is reachable **only from inside a larger heap object
//! that is itself moved** by the collection — so the copier must forward the container *and*,
//! while scanning it, forward the handle (the `[BinRef][ErlSubBits]` pair) and leave the
//! off-heap pointer word the same.
//!
//! Two cases, each driven through **both** strategies (`AnyCollector::Copying` and
//! `AnyCollector::Immix`, always pinned explicitly so the automatic rule cannot swap them):
//!
//! * **Case A — refc binary.** The 8-word on-heap pair is element 0 of a 3-tuple root; the
//!   `RefcBinary` payload is registered in the [`OffHeap`] list. Several minors and a major
//!   must relocate the container *and* the pair, keep the off-heap `val` pointer and the
//!   (empty) chain link byte-identical, and never move the reference count; then dropping the
//!   root and sweeping releases the payload **exactly once** (count → the test's single
//!   reference; the buffer freed exactly once — observed through the allocator's live-block
//!   count and free-call counter).
//! * **Case B — MSO.** An MSO has no on-heap reference *term*: its safe reference is the
//!   [`OffHeap::insert`] slot index (a small integer) inside an ordinary term. That slot lives
//!   in a 2-tuple root, so the reference sits inside an object the collector moves. Across the
//!   same collections the tuple is relocated, the slot still resolves and the payload's
//!   destructor must run **exactly once** when the entry is swept dead (and never again).
//!
//! **Out of scope here (cross-crate).** The ETS- and `persistent_term`-style *sharing* paths
//! that keep large binaries / literals alive through `beam_ets`/`beam_bifs` cannot be reached
//! from `beam_gc` (they live in sibling crates and need their own liveness walks) and are not
//! exercised by this crate's tests. What is proven here is the collector's own half of the
//! contract: a moved handle never disturbs the off-heap word, and the caller-side sweep
//! reclaims the payload exactly once.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or that a
//! collection just wrote); every block carries a `// SAFETY:` rationale. The helper shapes
//! follow `tests/{gc_pressure_offheap,collector_adversarial,refc_binary_copy}.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_alloc::{AllocRef, AllocatorConfig, CarrierAllocator, Heap, RefcBinary};
use beam_gc::prelude::{
    AnyCollector, GcConfig, HeapPolicy, MatureKind, Mso, OffHeap, OffHeapEntry,
};
use beam_term::prelude::{
    arityval, bin_ref_parts, boxed_val, is_arity_value, is_atom, is_boxed, is_list, is_refc_binary,
    is_small, list_val, make_small, refc_bytes, refc_size, signed_value, write_refc_binary, Eterm,
    ERL_REFC_BITS_SIZE, NIL,
};

/// The off-heap payload of the refc binary in case A.
const REFC_BYTES: &[u8] = b"an off-heap refc binary reachable only from inside a moved object";
/// The apparent size charged for the MSO in case B.
const MSO_APPARENT: usize = 256;
/// The number of minors every case drives before its major.
const MINORS: usize = 3;

// ---------------------------------------------------------------------------
// Collector / policy scaffolding (the explicit-pin pattern of `collector_adversarial.rs`)
// ---------------------------------------------------------------------------

/// The two strategies, in a fixed order.
const BOTH: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// A policy whose **explicit** mature size is tuned per strategy.
///
/// The mark-region space rounds every object up to a whole 16-word line, so it needs a larger
/// *word* budget for the same graph than the copying space does. `min_heap_words` is the
/// nursery size, so the nursery is never shrunk below it by the bounded re-size rule.
fn policy_for(
    kind: MatureKind,
    young: usize,
    copying_mature: usize,
    immix_mature: usize,
) -> HeapPolicy {
    let mature = match kind {
        MatureKind::Copying => copying_mature,
        MatureKind::Immix => immix_mature,
    };
    HeapPolicy {
        initial_young_words: young,
        initial_mature_words: mature,
        min_heap_words: young,
        ..HeapPolicy::default()
    }
}

/// Builds the collector for `kind` through the **explicit** pin, and checks the pin really
/// beat the automatic rule.
fn collector(kind: MatureKind, policy: &HeapPolicy) -> AnyCollector {
    let config = match kind {
        MatureKind::Copying => GcConfig::copying(policy.clone()),
        MatureKind::Immix => GcConfig::immix(policy.clone()),
    };
    let c = AnyCollector::from_config(&config).expect("collector construction");
    assert_eq!(
        c.kind(),
        kind,
        "the explicit pin must select the requested strategy"
    );
    c
}

/// Whether `term` is resident in the strategy's mature generation.
fn in_mature(c: &AnyCollector, term: Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains_term(term),
        AnyCollector::Immix(x) => x.mature().contains_term(term),
    }
}

/// The post-collection invariants every strategy must show: an empty nursery and every root
/// relocated out of it into the mature generation.
fn assert_collected_no_leak(c: &AnyCollector, roots: &[Eterm], ctx: &str) {
    assert_eq!(
        c.young().used_words(),
        0,
        "{ctx}: the nursery must be empty after a collection"
    );
    for (i, root) in roots.iter().enumerate() {
        assert!(
            !c.young().contains_term(*root),
            "{ctx}: root {i} stayed in the nursery"
        );
        assert!(
            in_mature(c, *root),
            "{ctx}: root {i} was not relocated into the mature generation"
        );
    }
}

/// An isolated carrier allocator, so a case can observe a binary's payload being freed
/// through `AllocStats` without racing the process-wide registry allocator.
fn test_allocator(name: &str) -> AllocRef {
    CarrierAllocator::new(AllocatorConfig::builder().name(name).build())
        .expect("allocator construction maps nothing")
        .share()
}

// ---------------------------------------------------------------------------
// Off-heap reference helpers
// ---------------------------------------------------------------------------

/// Writes the 8-word `[BinRef][ErlSubBits]` pair for `payload` at the heap top and returns
/// its root (the `ErlSubBits`).
fn write_refc(heap: &mut Heap, payload: &RefcBinary) -> Eterm {
    let size_bits = payload.len() * 8;
    // SAFETY: exactly `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved;
    // `payload.data_ptr()` is a live, word-aligned pointer to `size_bits` bits of off-heap
    // storage kept alive by `payload` (held by the caller for the whole test), and `null` is
    // the (empty) off-heap chain link.
    unsafe {
        let hp = heap
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("8 words for the pair");
        write_refc_binary(
            hp,
            payload.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.data_ptr(),
            0,
            size_bits,
        )
    }
}

/// Records the off-heap payload pointer of every refc binary reachable from `term`.
///
/// This is the process-side liveness walk the collector deliberately does *not* do: it
/// enumerates the on-heap references that keep a registered off-heap object alive. These cases
/// only build tuples and refc binaries, so the walk handles exactly those shapes.
fn collect_refc_payloads(term: Eterm, out: &mut Vec<*mut u8>) {
    if term == NIL || is_small(term) || is_atom(term) {
        return;
    }
    if is_list(term) {
        // SAFETY: `term` is a live cons cell, so both words are readable.
        let cell = list_val(term);
        collect_refc_payloads(unsafe { *cell }, out);
        collect_refc_payloads(unsafe { *cell.add(1) }, out);
        return;
    }
    if !is_boxed(term) {
        return;
    }
    // SAFETY: `term` is a live boxed term; `is_refc_binary` reads only its header/`orig`.
    if unsafe { is_refc_binary(term) } {
        // SAFETY: `term` is a live refc binary; `orig` (word 4) is a live `BinRef` term.
        let orig = unsafe { *boxed_val(term).add(4) };
        // SAFETY: `orig` is a live `BinRef` term, so `bin_ref_parts` may read words 1 and 2.
        let (val, _next) = unsafe { bin_ref_parts(orig) };
        out.push(val as *mut u8);
        return;
    }
    // SAFETY: `term` is a live boxed term, so its header word is readable.
    let header = unsafe { *boxed_val(term) };
    if is_arity_value(header) {
        let n = arityval(header);
        for i in 0..n {
            // SAFETY: element `i` lies inside the live `n`-element tuple just validated.
            collect_refc_payloads(unsafe { *boxed_val(term).add(1 + i) }, out);
        }
    }
}

/// The set of off-heap payload pointers the live roots still reference.
fn live_payloads(roots: &[Eterm]) -> Vec<*mut u8> {
    let mut out = Vec::new();
    for &root in roots {
        collect_refc_payloads(root, &mut out);
    }
    out
}

/// The liveness predicate an owning process passes to [`OffHeap::sweep`]: a binary entry is
/// live iff a reachable on-heap reference names its payload (these cases register no MSO in
/// the same list, so anything else is treated as live).
fn binary_is_live(entry: &OffHeapEntry, live: &[*mut u8]) -> bool {
    match entry.as_binary() {
        Some(binary) => live.contains(&binary.data_ptr()),
        None => true,
    }
}

// ---------------------------------------------------------------------------
// Case A: an off-heap refc binary reached through a moved container tuple
// ---------------------------------------------------------------------------

/// Builds the root container: the 3-tuple `{handle, 42, {7, 8}}` whose **first element** is the
/// on-heap off-heap handle. The tuple is the sole GC root, so the handle is reachable only
/// from inside an object the collector must move.
fn container_with(heap: &mut Heap, handle: Eterm) -> Eterm {
    let nested = heap
        .tuple(&[make_small(7), make_small(8)])
        .expect("nested tuple");
    heap.tuple(&[handle, make_small(42), nested])
        .expect("container tuple")
}

/// Reads element 0 of the container tuple (the on-heap off-heap handle).
fn container_handle(container: Eterm) -> Eterm {
    // SAFETY: `container` is a live boxed term the caller built; its header word is readable.
    let header = unsafe { *boxed_val(container) };
    assert!(
        is_arity_value(header),
        "the root must be our container tuple"
    );
    assert_eq!(arityval(header), 3, "the container is a 3-tuple");
    // SAFETY: element 0 lies inside the live 3-element tuple just validated.
    unsafe { *boxed_val(container).add(1) }
}

/// Checks the container survived a move with its non-handle elements intact (`42` and `{7,8}`).
fn assert_container_shape(container: Eterm, ctx: &str) {
    // SAFETY: `container` is a live 3-tuple root.
    let header = unsafe { *boxed_val(container) };
    assert!(is_arity_value(header), "{ctx}: the container is a tuple");
    assert_eq!(arityval(header), 3, "{ctx}: the container is a 3-tuple");

    // SAFETY: element 1 lies inside the live 3-element tuple just validated.
    let scalar = unsafe { *boxed_val(container).add(2) };
    assert!(
        is_small(scalar),
        "{ctx}: element 1 must stay a small integer"
    );
    assert_eq!(
        signed_value(scalar),
        42,
        "{ctx}: element 1 must be preserved"
    );

    // SAFETY: element 2 lies inside the live 3-element tuple just validated.
    let nested = unsafe { *boxed_val(container).add(3) };
    assert!(
        is_boxed(nested),
        "{ctx}: element 2 must stay the nested tuple"
    );
    // SAFETY: `nested` is a live boxed term.
    let nh = unsafe { *boxed_val(nested) };
    assert!(is_arity_value(nh), "{ctx}: element 2 must stay a tuple");
    assert_eq!(arityval(nh), 2, "{ctx}: the nested tuple is a 2-tuple");
    // SAFETY: elements 0 and 1 lie inside the live 2-element tuple just validated.
    let (n0, n1) = unsafe { (*boxed_val(nested).add(1), *boxed_val(nested).add(2)) };
    assert!(
        is_small(n0) && is_small(n1),
        "{ctx}: the nested elements are small integers"
    );
    assert_eq!(
        (signed_value(n0), signed_value(n1)),
        (7, 8),
        "{ctx}: the nested elements must be preserved"
    );
}

/// The per-collection assertions for case A: the container **and** the handle inside it were
/// moved to fresh addresses, the handle still describes the very same off-heap payload with a
/// byte-identical `val`/`next`, and the container's other elements survived.
fn assert_moved_handle_intact(
    c: &AnyCollector,
    roots: &[Eterm],
    handle_before: usize,
    container_before: usize,
    val_word_before: usize,
    payload: &RefcBinary,
    ctx: &str,
) {
    assert_collected_no_leak(c, roots, ctx);

    let container = roots[0];
    // The container's address after the move (its term is the live root).
    let container_after = boxed_val(container) as usize;
    assert_ne!(
        container_after, container_before,
        "{ctx}: the container root must have moved to a new address"
    );
    assert_container_shape(container, ctx);

    // The handle (element 0) was forwarded into the mature generation, at a new address — the
    // *only* on-heap reference to the off-heap object moved with it.
    let handle = container_handle(container);
    assert!(
        in_mature(c, handle),
        "{ctx}: the handle must be relocated into the mature generation"
    );
    // The handle's address after the move (it is the live pair element).
    let handle_after = boxed_val(handle) as usize;
    assert_ne!(
        handle_after, handle_before,
        "{ctx}: the handle inside the moved container must itself have moved"
    );

    // It is still a refc binary describing the same payload window and bytes.
    // SAFETY: `handle` is the live, copied `ErlSubBits`.
    unsafe {
        assert!(
            is_refc_binary(handle),
            "{ctx}: the handle is still a refc binary"
        );
        assert_eq!(
            refc_size(handle),
            payload.len() * 8,
            "{ctx}: the window size is preserved"
        );
        assert_eq!(
            refc_bytes(handle),
            REFC_BYTES,
            "{ctx}: the payload bytes are untouched"
        );
    }

    // The `orig` BinRef was forwarded too, and its off-heap pointer word is **byte-identical**:
    // a collection never moves or rewrites the payload reference.
    // SAFETY: `handle` is the live copied `ErlSubBits`, so word 4 is in-bounds.
    let orig = unsafe { *boxed_val(handle).add(4) };
    assert!(
        in_mature(c, orig),
        "{ctx}: the BinRef must be forwarded as well"
    );
    // SAFETY: `orig` is the live forwarded `BinRef`, so words 1 and 2 are in-bounds.
    let (val, next) = unsafe { bin_ref_parts(orig) };
    assert_eq!(
        val as *mut u8,
        payload.data_ptr(),
        "{ctx}: the off-heap pointer must be left byte-identical"
    );
    assert_eq!(
        val as usize, val_word_before,
        "{ctx}: the `BinRef.val` word the collection carried over must be bit-for-bit the one \
         written in the nursery"
    );
    assert_eq!(
        next as usize, 0,
        "{ctx}: the (empty) off-heap chain link must be left byte-identical"
    );
}

/// An off-heap refc binary reachable **only** from a container tuple that a collection moves
/// survives several minors and a major with its refcount stable, its off-heap pointer
/// byte-identical and its on-heap handle relocated — and is released exactly once when the
/// container becomes garbage behind a caller-supplied liveness predicate.
#[test]
fn a_refc_binary_handle_inside_a_moved_object_is_reclaimed_exactly_once() {
    for kind in BOTH {
        let alloc = test_allocator(match kind {
            MatureKind::Copying => "offheap-liveness-refc-copy",
            MatureKind::Immix => "offheap-liveness-refc-immix",
        });
        let payload = RefcBinary::with_allocator(alloc.clone(), REFC_BYTES).expect("payload");
        assert_eq!(alloc.stats().live_blocks, 1, "{kind:?}: one live buffer");
        assert_eq!(payload.refcount(), 1, "{kind:?}");

        // The off-heap list owns a second reference (slot 0); the test keeps its own, so the
        // single decrement a sweep performs is observable.
        let mut off_heap = OffHeap::new();
        let slot = off_heap.insert(payload.clone());
        assert_eq!(slot, 0, "{kind:?}: the first slot");
        assert_eq!(payload.refcount(), 2, "{kind:?}");
        assert_eq!(off_heap.overhead_bytes(), REFC_BYTES.len(), "{kind:?}");
        assert_eq!(off_heap.len(), 1, "{kind:?}");

        let policy = policy_for(kind, 4096, 8192, 32 * 1024);
        let mut c = collector(kind, &policy);

        // The on-heap handle is element 0 of the sole root, a 3-tuple the collector must move.
        let container;
        let handle_before;
        let val_word_before;
        {
            let heap = c.young_mut().heap_mut();
            let handle = write_refc(heap, &payload);
            container = container_with(heap, handle);
            // The pair's address in the nursery, before any collection.
            handle_before = boxed_val(handle) as usize;
            // The off-heap pointer word (`BinRef.val`) exactly as it stands in the nursery.
            // SAFETY: `handle` is the live `ErlSubBits` just written, so its `orig` (word 4) is
            // a live `BinRef` whose `val` word (word 1) is in-bounds.
            let orig = unsafe { *boxed_val(handle).add(4) };
            // SAFETY: `orig` is the live `BinRef` term just written.
            val_word_before = unsafe { *boxed_val(orig).add(1) }.raw() as usize;
        }
        // The pre-collection word really names the payload (so "unchanged afterwards" means
        // "still the same off-heap pointer").
        assert_eq!(
            val_word_before,
            payload.data_ptr() as usize,
            "{kind:?}: the nursery's `BinRef.val` word names the off-heap payload"
        );
        // The container's address in the nursery, before any collection.
        let container_before = boxed_val(container) as usize;
        let mut roots = vec![container];

        // Several minors: each relocates the container + the handle it holds, keeps the
        // off-heap word byte-identical and never touches the reference count.
        for round in 0..MINORS {
            c.collect_minor(&mut roots).expect("minor");
            let ctx = format!("{kind:?} minor {round}");
            assert_moved_handle_intact(
                &c,
                &roots,
                handle_before,
                container_before,
                val_word_before,
                &payload,
                &ctx,
            );

            let live = live_payloads(&roots);
            assert_eq!(
                live,
                vec![payload.data_ptr()],
                "{ctx}: the moved handle still names the payload"
            );
            assert_eq!(
                off_heap.sweep(|e| binary_is_live(e, &live)),
                0,
                "{ctx}: a live handle keeps the off-heap entry"
            );
            assert_eq!(
                payload.refcount(),
                2,
                "{ctx}: the refcount is stable across the move"
            );
        }

        // A major (fullsweep) through both strategies: the container and its handle are
        // evacuated again and the off-heap pointer stays verbatim.
        c.collect_major(&mut roots).expect("major");
        let ctx = format!("{kind:?} major");
        assert_moved_handle_intact(
            &c,
            &roots,
            handle_before,
            container_before,
            val_word_before,
            &payload,
            &ctx,
        );
        let live = live_payloads(&roots);
        assert_eq!(live, vec![payload.data_ptr()], "{ctx}");
        assert_eq!(
            off_heap.sweep(|e| binary_is_live(e, &live)),
            0,
            "{ctx}: still reachable, so still kept"
        );
        assert_eq!(payload.refcount(), 2, "{ctx}");
        assert_eq!(off_heap.len(), 1, "{ctx}");

        // Phase 2: the container is dropped from the roots, so a fullsweep reclaims the pair
        // (and the handle inside it) and the now-unreachable off-heap entry is released
        // **exactly once** through the caller-side predicate.
        let mut none: [Eterm; 0] = [];
        c.collect_major(&mut none).expect("major");
        assert_eq!(c.young().used_words(), 0, "{kind:?}: the nursery is empty");
        let live = live_payloads(&none);
        assert!(
            live.is_empty(),
            "{kind:?}: no reachable handle names the payload any more"
        );
        assert_eq!(
            off_heap.sweep(|e| binary_is_live(e, &live)),
            1,
            "{kind:?}: the dead handle releases exactly one entry"
        );
        assert!(off_heap.is_empty(), "{kind:?}");
        assert_eq!(
            payload.refcount(),
            1,
            "{kind:?}: the release decremented the count exactly once"
        );
        // The payload is still readable through the surviving test handle: no use-after-free.
        assert_eq!(
            payload.as_bytes(),
            REFC_BYTES,
            "{kind:?}: the payload bytes are intact"
        );

        // Dropping the last reference frees the buffer exactly once — a leak would leave a live
        // block, a double free would trip the allocator's plausibility check.
        let before = alloc.stats();
        assert_eq!(before.live_blocks, 1, "{kind:?}");
        drop(payload);
        let after = alloc.stats();
        assert_eq!(
            after.live_blocks, 0,
            "{kind:?}: the payload's buffer was freed"
        );
        assert_eq!(
            after.free_calls,
            before.free_calls + 1,
            "{kind:?}: the buffer was freed exactly once"
        );
    }
}

// ---------------------------------------------------------------------------
// Case B: an off-heap MSO reached through a moved container tuple
// ---------------------------------------------------------------------------

/// An MSO payload that counts how many times it has been dropped.
struct DropPayload {
    drops: Arc<AtomicUsize>,
    tag: u32,
    words: Vec<i64>,
}

impl DropPayload {
    fn new(drops: &Arc<AtomicUsize>) -> DropPayload {
        DropPayload {
            drops: Arc::clone(drops),
            tag: 0xABCD,
            words: vec![10, 20, 30, 40],
        }
    }
}

impl Drop for DropPayload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// The per-collection assertions for case B: the container tuple was moved and still carries
/// the slot index, the slot still resolves to the same un-dropped, intact payload, and the
/// destructor has not run.
fn assert_moved_mso_slot_intact(
    c: &AnyCollector,
    roots: &[Eterm],
    container_before: usize,
    slot: usize,
    off_heap: &OffHeap,
    drops: &Arc<AtomicUsize>,
    ctx: &str,
) {
    assert_collected_no_leak(c, roots, ctx);

    let container = roots[0];
    // The container's address after the move (its term is the live root).
    let container_after = boxed_val(container) as usize;
    assert_ne!(
        container_after, container_before,
        "{ctx}: the container must have moved to a new address"
    );

    // The tuple still holds `{slot, 7}`; the slot index is a small integer, carried verbatim
    // inside the moved container.
    // SAFETY: `container` is the live 2-tuple root, so its header and both elements are live.
    let header = unsafe { *boxed_val(container) };
    assert!(is_arity_value(header), "{ctx}: the container is a tuple");
    assert_eq!(arityval(header), 2, "{ctx}: the container is a 2-tuple");
    // SAFETY: elements 0 and 1 lie inside the live 2-element tuple just validated.
    let (e0, e1) = unsafe { (*boxed_val(container).add(1), *boxed_val(container).add(2)) };
    assert!(
        is_small(e0) && is_small(e1),
        "{ctx}: both elements are small integers"
    );
    assert_eq!(
        (signed_value(e0), signed_value(e1)),
        (slot as i64, 7),
        "{ctx}: the slot index inside the moved container is preserved"
    );

    // The slot still resolves and the payload is intact — nothing was dropped or released.
    assert_eq!(
        drops.load(Ordering::SeqCst),
        0,
        "{ctx}: nothing must be dropped yet"
    );
    let entry = off_heap.get(slot).expect("the slot still resolves");
    let mso = entry.as_mso().expect("still an MSO entry");
    assert_eq!(
        mso.refcount(),
        1,
        "{ctx}: the refcount is stable across the move"
    );
    assert_eq!(mso.apparent_size(), MSO_APPARENT, "{ctx}");
    let payload = mso.downcast_ref::<DropPayload>().expect("the payload type");
    assert_eq!(payload.tag, 0xABCD, "{ctx}: the payload is intact");
    assert_eq!(
        payload.words,
        vec![10, 20, 30, 40],
        "{ctx}: the payload is intact"
    );
}

/// An off-heap MSO referenced only through the slot index carried **inside a moved container
/// tuple** is untouched across several minors and a major (payload intact, refcount stable) and
/// its destructor runs **exactly once** when it is swept dead.
#[test]
fn an_mso_slot_inside_a_moved_object_releases_the_payload_exactly_once() {
    for kind in BOTH {
        let drops = Arc::new(AtomicUsize::new(0));
        let handle = Mso::new(DropPayload::new(&drops), MSO_APPARENT);
        assert_eq!(handle.refcount(), 1, "{kind:?}");

        let mut off_heap = OffHeap::new();
        let slot = off_heap.insert(handle.clone());
        assert_eq!(slot, 0, "{kind:?}");
        assert_eq!(handle.refcount(), 2, "{kind:?}");
        // Hand the list the *only* reference, so the release a sweep performs is the one that
        // runs the destructor.
        drop(handle);
        assert_eq!(
            off_heap
                .get(slot)
                .expect("the entry is present")
                .as_mso()
                .expect("an MSO entry")
                .refcount(),
            1,
            "{kind:?}"
        );
        assert_eq!(off_heap.overhead_bytes(), MSO_APPARENT, "{kind:?}");

        let policy = policy_for(kind, 4096, 8192, 32 * 1024);
        let mut c = collector(kind, &policy);

        // The on-heap reference to an MSO is the slot index (a small integer) inside an
        // ordinary term; here it is element 0 of the sole root, so the reference lives inside
        // an object the collector moves.
        let container = c
            .young_mut()
            .heap_mut()
            .tuple(&[make_small(slot as i64), make_small(7)])
            .expect("holder tuple");
        // The container's address in the nursery, before any collection.
        let container_before = boxed_val(container) as usize;
        let mut roots = vec![container];

        for round in 0..MINORS {
            c.collect_minor(&mut roots).expect("minor");
            let ctx = format!("{kind:?} minor {round}");
            assert_moved_mso_slot_intact(
                &c,
                &roots,
                container_before,
                slot,
                &off_heap,
                &drops,
                &ctx,
            );
        }
        c.collect_major(&mut roots).expect("major");
        let ctx = format!("{kind:?} major");
        assert_moved_mso_slot_intact(&c, &roots, container_before, slot, &off_heap, &drops, &ctx);

        // Swept dead: exactly one release, the destructor runs exactly once, and nothing runs
        // it again.
        assert_eq!(
            off_heap.sweep(|_| false),
            1,
            "{kind:?}: exactly one entry released"
        );
        assert!(off_heap.is_empty(), "{kind:?}");
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "{kind:?}: the destructor must run exactly once"
        );
        assert_eq!(
            off_heap.sweep(|_| false),
            0,
            "{kind:?}: nothing left to sweep"
        );
        drop(off_heap);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "{kind:?}: the destructor must not run twice"
        );
    }
}
