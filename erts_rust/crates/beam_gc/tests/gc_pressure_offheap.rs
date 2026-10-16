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

//! **Correctness under pressure** — off-heap refcounted binaries across a collection
//! (deliverable 3, scenarios 2 and 3).
//!
//! The collector never walks an off-heap payload: it copies only the small on-heap *handle*
//! (the 8-word `[BinRef][ErlSubBits]` pair) and leaves the payload pointer verbatim. Whether
//! the off-heap object survives is therefore decided by the process that owns the
//! [`OffHeap`] list, which after a collection sweeps every entry whose on-heap reference the
//! collection found dead. These tests pin the two ends of that contract:
//!
//! * **Scenario 2** — an off-heap `RefcBinary` whose on-heap handle becomes garbage is
//!   swept exactly once: its reference count is decremented once and the buffer is freed
//!   exactly once (counted through an isolated allocator's `live_blocks`/`free_calls`), with
//!   no leak and no double free. A companion test proves the sweep is *selective*: a live
//!   handle keeps its entry while the dead one is released.
//! * **Scenario 3** — a `Local` fun whose environment holds a refc-binary term keeps the
//!   off-heap binary alive for as long as the fun is live (the fun block + environment are
//!   relocated and the binary's payload is left byte-identical), and releases it exactly
//!   once when the fun itself becomes garbage.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or that a
//! collection just wrote); every block carries a `// SAFETY:` rationale.

use beam_alloc::{AllocRef, AllocatorConfig, CarrierAllocator, Heap, RefcBinary};
use beam_gc::prelude::{CollectionKind, Collector, HeapPolicy, OffHeap, OffHeapEntry};
use beam_term::prelude::{
    arityval, bin_ref_parts, boxed_val, fun_env, fun_words, is_arity_value, is_atom, is_boxed,
    is_fun_header, is_list, is_refc_binary, is_small, list_val, make_small, refc_bytes,
    write_fun_local, write_refc_binary, Eterm, FunKind, ERL_REFC_BITS_SIZE, NIL,
};

/// The off-heap payload of the scenario-2 binary.
const DIES_BYTES: &[u8] = b"a refc binary that dies mid-collection, off the process heap";
/// The off-heap payload of the scenario-3 binary (held by a fun environment).
const FUN_REFC_BYTES: &[u8] = b"a refc binary held only by a fun environment";

/// A policy with the given cadence and explicit initial sizes.
fn policy(cadence: u32, young: usize, mature: usize) -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: cadence,
        initial_young_words: young,
        initial_mature_words: mature,
        ..HeapPolicy::default()
    }
}

/// An isolated carrier allocator, so a test can observe a binary's payload being freed
/// through `AllocStats` without racing the process-wide registry allocator.
fn test_allocator(name: &str) -> AllocRef {
    CarrierAllocator::new(AllocatorConfig::builder().name(name).build())
        .expect("allocator construction maps nothing")
        .share()
}

/// Writes the 8-word `[BinRef][ErlSubBits]` pair for `payload` at the heap top.
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
/// enumerates the on-heap references that keep a registered off-heap object alive. These
/// tests only build tuples, cons lists, funs and refc binaries, so the walk handles exactly
/// those shapes.
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
    // SAFETY: `term` is a live boxed term, so `is_refc_binary` may read its header.
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
    if is_fun_header(header) {
        // SAFETY: `term` is a live fun, so its environment words are readable.
        if let Ok(env) = unsafe { fun_env(term) } {
            for entry in env {
                collect_refc_payloads(*entry, out);
            }
        }
        return;
    }
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
/// live iff one of the reachable on-heap references points at its payload.
fn entry_is_live(entry: &OffHeapEntry, live: &[*mut u8]) -> bool {
    match entry.as_binary() {
        Some(binary) => live.contains(&binary.data_ptr()),
        // These tests register no generic MSOs; anything else is treated as live.
        None => true,
    }
}

// ---------------------------------------------------------------------------
// Scenario 2: an off-heap refc binary that dies mid-collection
// ---------------------------------------------------------------------------

#[test]
fn a_refc_binary_whose_onheap_handle_becomes_garbage_is_released_exactly_once() {
    let alloc = test_allocator("gc-pressure-refc-dies");
    let payload = RefcBinary::with_allocator(alloc.clone(), DIES_BYTES).expect("payload");
    assert_eq!(
        alloc.stats().live_blocks,
        1,
        "the payload's buffer is one live block"
    );
    assert_eq!(payload.refcount(), 1);

    // Register a clone: the off-heap list owns one reference and the test keeps another, so
    // the single decrement the sweep performs is observable.
    let mut off_heap = OffHeap::new();
    off_heap.insert(payload.clone());
    assert_eq!(off_heap.len(), 1);
    assert_eq!(payload.refcount(), 2);

    // The on-heap handle: the 8-word `[BinRef][ErlSubBits]` pair in the nursery.
    let mut collector = Collector::new(policy(1, 4096, 8192)).expect("collector");
    let handle = write_refc(collector.young_mut().heap_mut(), &payload);

    // Phase 1: the handle is a live root. The minor promotes it; the off-heap entry is
    // retained because a reachable reference still names the payload.
    let mut roots = [handle];
    let first = collector.collect(&mut roots).expect("collect");
    assert_eq!(first.kind, CollectionKind::Minor);
    assert!(collector.mature().contains_term(roots[0]));
    // SAFETY: the copied handle is still a live refc binary describing `payload`.
    assert!(unsafe { is_refc_binary(roots[0]) });
    let live = live_payloads(&roots);
    assert_eq!(
        live,
        vec![payload.data_ptr()],
        "the walk finds the live handle"
    );
    assert_eq!(
        off_heap.sweep(|e| entry_is_live(e, &live)),
        0,
        "a live handle keeps the off-heap entry"
    );
    assert_eq!(off_heap.len(), 1);
    assert_eq!(
        payload.refcount(),
        2,
        "the retained entry still holds its reference"
    );

    // Phase 2: the handle is dropped from the roots, so the fullsweep reclaims the on-heap
    // pair; the now-unreachable off-heap entry is swept exactly once.
    let mut no_roots: [Eterm; 0] = [];
    let second = collector.collect(&mut no_roots).expect("collect");
    assert_eq!(
        second.kind,
        CollectionKind::Major,
        "a fullsweep reclaims the pair"
    );
    let live = live_payloads(&no_roots);
    assert!(
        live.is_empty(),
        "no reachable handle references the payload any more"
    );
    assert_eq!(
        off_heap.sweep(|e| entry_is_live(e, &live)),
        1,
        "the dead handle releases exactly one entry"
    );
    assert!(off_heap.is_empty());
    assert_eq!(
        payload.refcount(),
        1,
        "the release decremented the count exactly once"
    );
    // The payload is still readable through the surviving test handle: no use-after-free.
    assert_eq!(
        payload.as_bytes(),
        DIES_BYTES,
        "the payload bytes are intact"
    );

    // Dropping the last reference frees the buffer exactly once — a leak would leave a live
    // block, and a double free would be caught by the allocator's plausibility check.
    let before = alloc.stats();
    assert_eq!(before.live_blocks, 1);
    drop(payload);
    let after = alloc.stats();
    assert_eq!(after.live_blocks, 0, "the payload's buffer was freed");
    assert_eq!(
        after.free_calls,
        before.free_calls + 1,
        "the buffer was freed exactly once"
    );
}

/// The sweep is **selective**: one live handle and one garbage handle over two registered
/// binaries releases exactly the dead one and leaves the live one's reference count intact.
#[test]
fn a_sweep_releases_only_the_garbage_binary_and_keeps_the_live_one() {
    let alloc = test_allocator("gc-pressure-refc-mixed");
    let live_payload = RefcBinary::with_allocator(alloc.clone(), b"live payload").expect("live");
    let dead_payload = RefcBinary::with_allocator(alloc.clone(), b"dead payload").expect("dead");
    assert_eq!(alloc.stats().live_blocks, 2);

    let mut off_heap = OffHeap::new();
    off_heap.insert(live_payload.clone());
    off_heap.insert(dead_payload.clone());

    let mut collector = Collector::new(policy(1, 4096, 8192)).expect("collector");
    let live_handle = write_refc(collector.young_mut().heap_mut(), &live_payload);
    // A dead handle: written into the nursery but never placed in a root, so the collection
    // reclaims it (the term word below is never dereferenced again).
    let _dead_handle = write_refc(collector.young_mut().heap_mut(), &dead_payload);

    let mut roots = [live_handle];
    collector.collect(&mut roots).expect("collect");
    let live = live_payloads(&roots);
    assert_eq!(
        live,
        vec![live_payload.data_ptr()],
        "only the live handle is reachable"
    );
    assert_eq!(
        off_heap.sweep(|e| entry_is_live(e, &live)),
        1,
        "exactly the garbage binary is released"
    );
    assert_eq!(off_heap.len(), 1);
    assert_eq!(
        live_payload.refcount(),
        2,
        "the live entry keeps its reference"
    );
    assert_eq!(
        dead_payload.refcount(),
        1,
        "the dead entry released its reference"
    );

    // Dropping the dead payload now frees its buffer exactly once; the live one remains.
    let before = alloc.stats();
    assert_eq!(before.live_blocks, 2);
    drop(dead_payload);
    assert_eq!(
        alloc.stats().live_blocks,
        1,
        "only the live payload's buffer remains"
    );

    // The live payload's test handle goes away, but the retained off-heap entry still holds
    // a reference, so the buffer stays alive until the list itself is dropped — at which
    // point it is freed exactly once.
    drop(live_payload);
    assert_eq!(
        alloc.stats().live_blocks,
        1,
        "the retained off-heap entry keeps the live buffer alive"
    );
    drop(off_heap);
    assert_eq!(
        alloc.stats().live_blocks,
        0,
        "releasing the retained entry frees the buffer exactly once"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: a fun whose environment holds a refc binary
// ---------------------------------------------------------------------------

#[test]
fn a_fun_holding_a_refc_binary_keeps_it_alive_and_releases_it_when_the_fun_dies() {
    let alloc = test_allocator("gc-pressure-fun-refc");
    let payload = RefcBinary::with_allocator(alloc.clone(), FUN_REFC_BYTES).expect("payload");
    let mut off_heap = OffHeap::new();
    off_heap.insert(payload.clone());
    assert_eq!(payload.refcount(), 2);

    let mut collector = Collector::new(policy(1, 4096, 8192)).expect("collector");

    // A `Local` fun whose environment is `[the refc binary, {1,2}]`.
    let fun = {
        let heap = collector.young_mut().heap_mut();
        let binary_term = write_refc(heap, &payload);
        let extra = heap.tuple(&[make_small(1), make_small(2)]).expect("extra");
        let words = fun_words(FunKind::Local, 2);
        // SAFETY: `words` exclusively-owned, writable words are reserved; all of them are
        // written by `write_fun_local`, and every env entry is a live term of this heap.
        unsafe {
            let hp = heap.alloc_words(words).expect("reserve local fun");
            write_fun_local(hp, 2, 0xABCD, 7, &[binary_term, extra]).expect("write local fun")
        }
    };

    // The fun is live, so a minor promotes it (with the refc pair it holds) and the off-heap
    // entry is retained.
    let mut roots = [fun];
    let first = collector.collect(&mut roots).expect("collect");
    assert_eq!(first.kind, CollectionKind::Minor);
    let copied = roots[0];
    assert!(collector.mature().contains_term(copied));

    // The fun block and its environment survived the move; env[0] is still the refc binary
    // and still describes the very same off-heap bytes.
    // SAFETY: `copied` is the live, copied local fun, so its environment is readable.
    let env = unsafe { fun_env(copied) }.expect("fun env");
    assert_eq!(env.len(), 2);
    // SAFETY: `env[0]` is a live term of the copied fun's environment.
    assert!(
        unsafe { is_refc_binary(env[0]) },
        "the refc binary term survived"
    );
    // SAFETY: `env[0]` is a live refc binary whose off-heap payload `payload` keeps alive.
    assert_eq!(unsafe { refc_bytes(env[0]) }, FUN_REFC_BYTES);

    let live = live_payloads(&roots);
    assert_eq!(
        live,
        vec![payload.data_ptr()],
        "the live fun keeps the off-heap binary reachable"
    );
    assert_eq!(
        off_heap.sweep(|e| entry_is_live(e, &live)),
        0,
        "the live fun keeps the off-heap entry"
    );
    assert_eq!(
        payload.refcount(),
        2,
        "the retained entry still holds its reference"
    );

    // Once the fun is garbage a fullsweep reclaims it (and the refc pair with it); the
    // off-heap binary is then released exactly once.
    let mut no_roots: [Eterm; 0] = [];
    let second = collector.collect(&mut no_roots).expect("collect");
    assert_eq!(second.kind, CollectionKind::Major);
    assert_eq!(
        collector.mature().used_words(),
        0,
        "the fun graph is reclaimed"
    );
    let live = live_payloads(&no_roots);
    assert!(live.is_empty(), "the fun no longer references the binary");
    assert_eq!(
        off_heap.sweep(|e| entry_is_live(e, &live)),
        1,
        "the dead fun releases its off-heap binary exactly once"
    );
    assert!(off_heap.is_empty());
    assert_eq!(payload.refcount(), 1, "one reference was released");

    // The final reference frees the buffer exactly once.
    let before = alloc.stats();
    assert_eq!(before.live_blocks, 1);
    drop(payload);
    assert_eq!(
        alloc.stats().live_blocks,
        0,
        "the payload's buffer was freed"
    );
}
