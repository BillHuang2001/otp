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

//! Heap-fragment evidence: a term graph built **outside** every process space copies
//! into a process heap intact, and a fragment's words and off-heap objects are
//! reclaimed exactly once when it dies.
//!
//! The C ground truth is `ErlHeapFragment` (`erts/emulator/beam/erl_message.h:198-205`)
//! and the collector-side half in `collect_live_heap_frags`/`move_one_area`
//! (`erl_gc.c:2455-2511`), `erts_copy_one_frag` (`erl_gc.c:2513-...`) and
//! `erts_move_multi_frags`/`move_one_frag` (`copy.c:2044-2111`). A fragment lives
//! **outside every process space**, so a collection cannot evacuate it by pointer
//! rewriting: [`FragmentChain::copy_roots_into`] (and its per-fragment
//! [`HeapFragment::copy_term`]/[`copy_term_into`] forms) copies the graph into the
//! process heap and rewrites the caller's root slot in place — the "copy in, then
//! collect" [fragment-rootset contract](beam_gc::fragment).
//!
//! What these tests pin:
//!
//! * **(a)** a term built in a fragment copies into a [`HeapSpace`] intact —
//!   structure/values preserved, the root relocated into the destination, nothing
//!   pointing back into the fragment, and the destination holding *exactly* the live
//!   graph's independently-known footprint (proving shared sub-terms are copied once);
//! * **(b)** a graph **spanning two fragments** copies correctly through the
//!   chain-level copy, with cross-fragment sharing preserved;
//! * **(c)** an off-heap [`RefcBinary`] referenced from a fragment survives with the
//!   correct refcounts and is released **exactly once** (no leak, no double free);
//! * **(d)** a fragment's words and off-heap entries are reclaimed when it is dropped,
//!   observed through [`FragmentCounters`] and the backing allocator's `AllocStats`;
//! * the edge cases: an immediate / an already-destination term is shared unchanged, a
//!   too-small destination reports a typed heap exhaustion (never a panic), and
//!   [`HeapFragment::copy_term`] *shares* off-heap ownership while `copy_term_into`
//!   *transfers* it.
//!
//! These tests exercise the public API only; every `unsafe` block carries a `// SAFETY:`
//! rationale and only touches words this test allocated itself. The isolated
//! [`CarrierAllocator`]s make "no leak" observable as `live_blocks == 0`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_alloc::{
    default_allocator, AllocRef, AllocatorConfig, CarrierAllocator, RefcBinary,
};
use beam_common::BeamError;
use beam_gc::prelude::{
    collect, FragmentChain, FragmentCounters, FragmentRoots, HeapFragment, HeapSpace, Mso, OffHeap,
    OffHeapEntry,
};
use beam_term::prelude::{
    is_refc_binary, list_val, make_small, refc_bytes, refc_size, signed_value, tuple_element,
    tuple_size, write_refc_binary, Eterm, ERL_REFC_BITS_SIZE, NIL,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Allocates an empty `words`-word semi-space over the default allocator.
fn space(words: usize) -> HeapSpace {
    HeapSpace::with_allocator(default_allocator().clone(), words).expect("heap allocation")
}

/// An isolated, carrier-backed allocator so a test can observe `live_blocks == 0` (the
/// leak/no-double-free proof) without noise from the process-wide registry allocators.
fn test_allocator(name: &str) -> AllocRef {
    let config = AllocatorConfig::builder()
        .name(name)
        .carrier_size(64 * 1024)
        .build();
    CarrierAllocator::new(config)
        .expect("allocator construction maps nothing")
        .share()
}

/// A payload that counts its own drops, so a test can prove an off-heap MSO is
/// destroyed **exactly once**.
struct DropCounter {
    drops: Arc<AtomicUsize>,
}

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// (a) fragment -> heap copy integrity
// ---------------------------------------------------------------------------

/// A term built entirely in a fragment copies into a process [`HeapSpace`] intact: the
/// root is relocated into the destination, the structure and every leaf value survive,
/// nothing points back into the fragment, and the destination holds exactly the live
/// graph's footprint.
///
/// The graph deliberately **shares** its `[1,2,3]` list between the inner and outer
/// tuple: the destination's `used_words` (13) can only hold the list *once*, so the
/// figure simultaneously proves the copy neither drops nor duplicates the shared
/// sub-term.
#[test]
fn a_term_built_in_a_fragment_copies_into_a_heap_intact() {
    let mut frag = HeapFragment::new(64).expect("fragment");

    // A `[1,2,3]` cons list (built tail-first), 3 cells = 6 words.
    let l3 = frag.heap_mut().cons(make_small(3), NIL).expect("cons");
    let l23 = frag.heap_mut().cons(make_small(2), l3).expect("cons");
    let list123 = frag.heap_mut().cons(make_small(1), l23).expect("cons");
    // `{100, [1,2,3]}` (3 words) and `{200, {100,[1,2,3]}, [1,2,3]}` (4 words). The list is
    // referenced twice — a sharing edge the copy must preserve.
    let inner = frag
        .heap_mut()
        .tuple(&[make_small(100), list123])
        .expect("inner tuple");
    let outer = frag
        .heap_mut()
        .tuple(&[make_small(200), inner, list123])
        .expect("outer tuple");

    // 6 (list) + 3 (inner) + 4 (outer) = 13 live words, 5 objects.
    const LIVE_WORDS: usize = 13;
    assert_eq!(frag.used_words(), LIVE_WORDS);
    assert!(frag.contains_term(outer));
    assert!(frag.contains_term(list123));

    let mut to = space(LIVE_WORDS);
    let relocated = frag.copy_term(outer, &mut to).expect("copy the fragment term");

    // (a1) The root was relocated into the destination, out of the fragment.
    assert!(
        to.contains_term(relocated),
        "the copied root must point into the destination"
    );
    assert!(
        !frag.contains_term(relocated),
        "the copied root must no longer point into the fragment"
    );
    assert_ne!(relocated, outer, "the copy lives at a new address");

    // (a2) The destination holds *exactly* the live graph: the shared list is one copy.
    assert_eq!(
        to.used_words(),
        LIVE_WORDS,
        "only the live 13-word graph is in the destination"
    );

    // (a3) Structure and leaf values round-trip.
    // SAFETY: `relocated`/`inner_copy`/`list_copy` are live terms in `to`, which the test
    // keeps unmoved; every `tuple_element`/`list_val` read is in-bounds for the shape the
    // test itself wrote.
    unsafe {
        assert_eq!(tuple_size(relocated), 3);
        assert_eq!(tuple_element(relocated, 0), make_small(200));

        let inner_copy = tuple_element(relocated, 1);
        let list_copy = tuple_element(relocated, 2);
        assert!(to.contains_term(inner_copy), "the inner tuple was copied");
        assert!(to.contains_term(list_copy), "the list was copied");
        assert_eq!(tuple_size(inner_copy), 2);
        assert_eq!(tuple_element(inner_copy, 0), make_small(100));
        assert_eq!(
            tuple_element(inner_copy, 1),
            list_copy,
            "the shared list must be copied exactly once"
        );

        // Walk the copied list from the relocated root: every cell is in the destination
        // and carries the original values.
        let mut cur = list_copy;
        let mut values = Vec::new();
        while cur != NIL {
            assert!(
                to.contains_term(cur),
                "every copied cons cell must be in the destination"
            );
            let cell = list_val(cur);
            values.push(signed_value(*cell));
            cur = *cell.add(1);
        }
        assert_eq!(values, vec![1, 2, 3]);
    }

    // (a4) The copy is non-destructive: the fragment is untouched and still usable.
    assert_eq!(frag.used_words(), LIVE_WORDS, "the fragment was only read");
    assert!(frag.contains_term(outer));
    assert!(frag.contains_term(list123));
}

/// An immediate, a term already in the destination and a term in *another* space are all
/// shared verbatim — a fragment copy never duplicates what it does not own.
#[test]
fn copy_shares_an_immediate_and_any_out_of_fragment_term_unchanged() {
    let frag = HeapFragment::new(8).expect("fragment");
    let mut to = space(8);

    // An immediate has nothing to relocate.
    assert_eq!(
        frag.copy_term(make_small(5), &mut to).expect("copy"),
        make_small(5)
    );
    assert_eq!(to.used_words(), 0, "an immediate copies nothing");

    // A term already resident in the destination is returned unchanged (no duplication).
    let local = to.heap_mut().tuple(&[make_small(1)]).expect("tuple");
    assert_eq!(
        frag.copy_term(local, &mut to).expect("copy"),
        local,
        "an already-destination term is shared verbatim"
    );
    assert_eq!(to.used_words(), 2, "nothing was duplicated");

    // A term resident in an unrelated fragment is outside this fragment: shared verbatim.
    let mut other = HeapFragment::new(8).expect("fragment");
    let foreign = other.heap_mut().tuple(&[make_small(9)]).expect("tuple");
    assert_eq!(
        frag.copy_term(foreign, &mut to).expect("copy"),
        foreign,
        "a foreign term is not this fragment's to copy"
    );
    assert!(
        other.contains_term(foreign),
        "the foreign term is untouched in its own fragment"
    );
}

/// A destination with too little room reports the crate's typed heap exhaustion (a
/// normal condition, never a panic) and leaves the destination a valid heap.
#[test]
fn a_destination_too_small_reports_a_typed_error() {
    let mut frag = HeapFragment::new(8).expect("fragment");
    let l = frag.heap_mut().cons(make_small(1), NIL).expect("cons");
    let list = frag.heap_mut().cons(make_small(2), l).expect("cons"); // 4 words

    let mut tiny = space(2);
    let err = frag
        .copy_term(list, &mut tiny)
        .expect_err("a 4-word graph cannot fit in a 2-word space");
    assert!(
        matches!(err, BeamError::Alloc(_)),
        "a too-small destination is a typed allocation failure, got {err:?}"
    );

    // The destination is still structurally valid, and the fragment was not consumed.
    assert!(tiny.used_words() <= tiny.capacity_words());
    assert!(frag.contains_term(list), "the fragment source is intact");
}

// ---------------------------------------------------------------------------
// (b) a graph spanning two fragments
// ---------------------------------------------------------------------------

/// A graph whose root lives in one fragment and whose shared sub-term lives in an
/// **earlier** fragment copies correctly through the chain-level copy-in, preserving the
/// cross-fragment sharing (the sub-term is copied once, so both elements of the root
/// tuple name the same destination address).
#[test]
fn a_graph_spanning_two_fragments_copies_with_sharing_preserved() {
    let mut chain = FragmentChain::new();

    // Fragment A owns the shared `[1,2]` list (4 words).
    let mut a = HeapFragment::new(8).expect("fragment A");
    let a3 = a.heap_mut().cons(make_small(2), NIL).expect("cons");
    let shared = a.heap_mut().cons(make_small(1), a3).expect("cons");
    chain.push(a);

    // Fragment B owns `{shared, shared}` (3 words) — two references *across* fragments.
    let mut b = HeapFragment::new(16).expect("fragment B");
    let outer = b.heap_mut().tuple(&[shared, shared]).expect("tuple");
    chain.push(b);

    assert_eq!(chain.len(), 2);
    assert!(chain.contains_term(outer), "the root lives in fragment B");
    assert!(chain.contains_term(shared), "the shared list lives in fragment A");

    // 4 (the shared list) + 3 (the root tuple) = 7 live words.
    let mut dst = space(7);
    let mut dst_off = OffHeap::new();
    let mut roots = [outer];
    let count = chain
        .copy_roots_into(&mut roots, &mut dst, &mut dst_off)
        .expect("chain copy-in");

    assert_eq!(count, 1, "the root slot was relocated");
    let relocated = roots[0];
    assert_ne!(relocated, outer, "the slot was rewritten in place");
    assert!(dst.contains_term(relocated));
    assert!(!chain.contains_term(relocated));
    assert_eq!(dst.used_words(), 7, "shared sub-term copied once");

    // SAFETY: `relocated` is the live, copied 2-tuple; the reads are in-bounds.
    unsafe {
        assert_eq!(tuple_size(relocated), 2);
        let e0 = tuple_element(relocated, 0);
        let e1 = tuple_element(relocated, 1);
        assert!(dst.contains_term(e0), "the cross-fragment list was copied in");
        assert_eq!(
            e0, e1,
            "a sub-term shared across fragments must be copied exactly once"
        );
        // The copied list is `[1,2]`: two cons cells, both relocated.
        let cell = list_val(e0);
        assert_eq!(signed_value(*cell), 1);
        let second = *cell.add(1);
        assert!(dst.contains_term(second), "the tail cell was copied");
        let tail = list_val(second);
        assert_eq!(signed_value(*tail), 2);
        assert_eq!(*tail.add(1), NIL, "the list terminates at NIL");
    }
}

// ---------------------------------------------------------------------------
// (c) an off-heap refc binary referenced from a fragment
// ---------------------------------------------------------------------------

/// A refc binary built off-heap and referenced by an on-heap `BinRef` inside a fragment
/// survives the copy-in with the correct refcounts, and is released **exactly once**.
///
/// The `copy_term_into` path is the C `erts_copy_one_frag` transfer: the destination
/// `OffHeap` gains the entry (refcount++) while the fragment releases its own
/// (refcount--), so the net count is unchanged and the fragment may be dropped without
/// freeing a live payload.
#[test]
fn off_heap_refc_binary_survives_the_fragment_copy_with_correct_refcounts() {
    let alloc = test_allocator("fragment-refc");
    let data: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();

    let payload = RefcBinary::with_allocator(alloc.clone(), &data).expect("payload");
    let probe = payload.clone(); // refcount 2: the payload handle + the probe

    let mut frag = HeapFragment::with_allocator(alloc.clone(), ERL_REFC_BITS_SIZE)
        .expect("fragment");
    // SAFETY: exactly `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved at `hp`
    // (the `write_refc_binary` contract); `payload.data_ptr()` is a live, word-aligned
    // pointer to `data.len() * 8` bits of off-heap storage kept alive by `probe`; `null` is
    // the (empty) off-heap chain link.
    let root = unsafe {
        let hp = frag
            .heap_mut()
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("8 words for the pair");
        write_refc_binary(
            hp,
            payload.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.data_ptr(),
            0,
            data.len() * 8,
        )
    };
    assert_eq!(frag.used_words(), ERL_REFC_BITS_SIZE);

    // The fragment owns an off-heap reference (ownership moves into the list, no clone).
    assert_eq!(
        frag.register_off_heap(payload),
        0,
        "the first registered entry gets slot 0"
    );
    assert_eq!(frag.off_heap_len(), 1);
    assert_eq!(frag.off_heap_bytes(), data.len());
    assert_eq!(probe.refcount(), 2, "the probe and the fragment's entry");

    let mut dst = space(ERL_REFC_BITS_SIZE);
    let mut dst_off = OffHeap::new();
    let relocated = frag
        .copy_term_into(root, &mut dst, &mut dst_off)
        .expect("copy-in");

    // (c1) The graph moved; the fragment no longer owns the payload.
    assert!(dst.contains_term(relocated));
    assert!(!frag.contains_term(relocated));
    assert_eq!(frag.off_heap_len(), 0, "off-heap ownership transferred");
    assert_eq!(frag.off_heap_bytes(), 0);

    // (c2) The transfer left the net refcount unchanged, and the entry landed in the
    // destination list.
    assert_eq!(probe.refcount(), 2, "a transfer keeps the net refcount");
    assert_eq!(dst_off.len(), 1);
    assert_eq!(
        dst_off
            .get(0)
            .and_then(OffHeapEntry::as_binary)
            .map(RefcBinary::refcount),
        Some(2)
    );

    // (c3) The copied term still describes the original off-heap payload byte-for-byte.
    // SAFETY: `relocated` is the live copied `ErlSubBits` and `probe` keeps its off-heap
    // payload alive for the whole test.
    unsafe {
        assert!(is_refc_binary(relocated));
        assert_eq!(refc_size(relocated), data.len() * 8);
        assert_eq!(refc_bytes(relocated), &data[..]);
    }

    // (c4) Releasing the destination's list releases exactly one reference; the probe's
    // reference keeps the payload alive.
    assert_eq!(dst_off.sweep(|_| false), 1);
    assert_eq!(probe.refcount(), 1, "the destination released its reference");

    // (c5) Dropping the (already emptied) fragment is a no-op for off-heap ownership.
    drop(frag);

    // (c6) Freeing the last handle frees the payload exactly once: one `free` call, and the
    // allocator's live accounting returns to zero (no leak, no double free).
    let before = alloc.stats();
    assert_eq!(
        before.live_blocks, 1,
        "only the caller's payload block remains live"
    );
    drop(probe);
    let after = alloc.stats();
    assert_eq!(
        after.free_calls,
        before.free_calls + 1,
        "the payload block was freed exactly once"
    );
    assert_eq!(after.live_blocks, 0, "no payload leaked");
    assert_eq!(after.live_bytes, 0);
}

/// [`HeapFragment::copy_term`] **shares** off-heap ownership (the fragment keeps the
/// payload) while [`HeapFragment::copy_term_into`] **transfers** it — the documented
/// distinction between the read-only and move forms, both leaving the net refcount
/// unchanged.
#[test]
fn copy_term_shares_off_heap_while_copy_term_into_transfers_it() {
    let alloc = test_allocator("fragment-off-heap-share");
    let payload = RefcBinary::with_allocator(alloc.clone(), b"a shared payload").expect("payload");
    let probe = payload.clone(); // refcount 2
    let len = probe.len();

    let mut frag =
        HeapFragment::with_allocator(alloc.clone(), ERL_REFC_BITS_SIZE).expect("fragment");
    // SAFETY: 8 reserved words, the `write_refc_binary` contract; the payload is live via
    // `probe`, and the chain link is null.
    let root = unsafe {
        let hp = frag
            .heap_mut()
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("8 words");
        write_refc_binary(
            hp,
            probe.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            probe.data_ptr(),
            0,
            len * 8,
        )
    };
    frag.register_off_heap(payload);
    assert_eq!(probe.refcount(), 2);

    // Room for both copies below.
    let mut dst = space(2 * ERL_REFC_BITS_SIZE);
    let dst_off = OffHeap::new();

    // The read-only form keeps the fragment's ownership.
    let _shared = frag.copy_term(root, &mut dst).expect("share copy");
    assert_eq!(frag.off_heap_len(), 1, "the fragment still owns the payload");
    assert_eq!(probe.refcount(), 2, "sharing never changes the count");
    assert!(dst_off.is_empty(), "sharing populates no destination list");

    // The move form transfers ownership.
    let mut dst_off = dst_off;
    let _moved = frag
        .copy_term_into(root, &mut dst, &mut dst_off)
        .expect("move copy");
    assert_eq!(frag.off_heap_len(), 0, "ownership transferred");
    assert_eq!(dst_off.len(), 1);
    assert_eq!(probe.refcount(), 2, "a transfer keeps the net count");

    // Everything is reclaimed: the fragment's heap block, the payload, and nothing else.
    drop(frag);
    drop(dst_off);
    assert_eq!(probe.refcount(), 1, "the destination released its reference");
    drop(probe);
    let stats = alloc.stats();
    assert_eq!(stats.live_blocks, 0, "no leak");
    assert_eq!(stats.live_bytes, 0);
}

// ---------------------------------------------------------------------------
// (d) reclamation / leak observability
// ---------------------------------------------------------------------------

/// Dropping a fragment reclaims its words (the backing `beam_alloc::Heap` block) **and**
/// its off-heap entries (the `RefcBinary` reference and the MSO payload), which the
/// [`FragmentCounters`] gauge and the allocator's `AllocStats` both report as zero.
#[test]
fn dropping_a_fragment_reclaims_its_words_and_off_heap() {
    let alloc = test_allocator("fragment-drop");
    let counters = FragmentCounters::new();
    let drops = Arc::new(AtomicUsize::new(0));

    let binary =
        RefcBinary::with_allocator(alloc.clone(), b"off-heap payload bytes").expect("payload");
    let binary_len = binary.len();

    {
        let mut frag = HeapFragment::with_allocator_and_counters(
            alloc.clone(),
            counters.clone(),
            32,
        )
        .expect("fragment");
        frag.heap_mut().tuple(&[make_small(1)]).expect("tuple");

        // The fragment takes ownership of one refc binary and one MSO.
        assert_eq!(frag.register_off_heap(binary.clone()), 0);
        assert_eq!(
            frag.register_off_heap(Mso::new(
                DropCounter {
                    drops: drops.clone()
                },
                48
            )),
            1
        );

        assert_eq!(frag.off_heap_len(), 2);
        assert_eq!(frag.off_heap_bytes(), binary_len + 48);
        assert_eq!(counters.created(), 1);
        assert_eq!(counters.live(), 1);
        assert_eq!(counters.words(), 32);
        assert_eq!(counters.off_heap_bytes(), binary_len + 48);
        assert_eq!(counters.off_heap_bytes_peak(), binary_len + 48);
        assert_eq!(binary.refcount(), 2, "the caller's clone plus the fragment's entry");
        assert_eq!(
            alloc.stats().live_blocks,
            2,
            "the fragment heap block + the binary payload block"
        );
    }

    // The fragment is gone: its words and both off-heap objects were released.
    assert_eq!(counters.dropped(), 1);
    assert_eq!(counters.live(), 0, "no fragment leaked");
    assert_eq!(counters.off_heap_bytes(), 0, "the off-heap gauge is back to zero");
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "the MSO payload was destroyed exactly once"
    );
    assert_eq!(
        binary.refcount(),
        1,
        "the fragment released its binary reference"
    );
    assert_eq!(
        alloc.stats().live_blocks,
        1,
        "only the caller's still-referenced payload block remains"
    );

    // Dropping the last reference frees that block exactly once.
    drop(binary);
    let stats = alloc.stats();
    assert_eq!(stats.live_blocks, 0, "no leak");
    assert_eq!(stats.live_bytes, 0);
}

// ---------------------------------------------------------------------------
// the documented rootset contract, end to end
// ---------------------------------------------------------------------------

/// The fragment-rootset contract in one flow: **materialize** a fragment root into the
/// process heap (rewriting the slot in place), **gather** the now-heap-resident slots
/// into the collector's flat root slice, run an ordinary [`collect`], and **scatter**
/// the relocated slots back — after which the root points into the new to-space.
#[test]
fn fragment_roots_materialize_then_survive_an_ordinary_collection() {
    let mut chain = FragmentChain::new();
    let mut frag = HeapFragment::new(8).expect("fragment");
    let tuple = frag
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("tuple");
    chain.push(frag);

    let mut roots = FragmentRoots::new();
    roots.push(tuple);
    roots.push(make_small(99)); // an immediate slot must never be relocated

    // Step 1: copy the fragment-resident slot into the nursery, in place.
    let mut nursery = space(16);
    let mut off_heap = OffHeap::new();
    let relocated = roots
        .materialize(&mut chain, &mut nursery, &mut off_heap)
        .expect("materialize");
    assert_eq!(relocated, 1, "only the fragment-resident slot moved");
    assert!(nursery.contains_term(roots.as_slice()[0]));
    assert!(!chain.contains_term(roots.as_slice()[0]));
    assert_eq!(
        roots.as_slice()[1],
        make_small(99),
        "an immediate slot is left untouched"
    );

    // Step 2: gather the heap-resident slots into the collector's flat root slice, and
    // collect exactly as any other roots would be collected.
    let mut flat: Vec<Eterm> = vec![make_small(-1)]; // some other root, e.g. x(0)
    let start = roots.gather_roots(&mut flat);
    assert_eq!(start, 1);
    let from = nursery;
    let mut to = space(16);
    collect(&from, &mut to, &mut flat).expect("collect");
    roots.scatter_roots(&flat, start);

    // The root survived the collection and now points into the to-space.
    let survivor = roots.as_slice()[0];
    assert!(
        to.contains_term(survivor),
        "the materialized root must survive the collection"
    );
    assert!(!from.contains_term(survivor));
    assert_eq!(roots.as_slice()[1], make_small(99));

    // SAFETY: `survivor` is the live, copied 2-tuple.
    unsafe {
        assert_eq!(tuple_size(survivor), 2);
        assert_eq!(tuple_element(survivor, 0), make_small(7));
        assert_eq!(tuple_element(survivor, 1), make_small(8));
    }
}
