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

//! **Slot stability of the per-process off-heap list** — the contract that makes
//! [`OffHeap::sweep`] usable by a production consumer.
//!
//! An on-heap off-heap reference stores a **slot index** into the owning process's off-heap
//! list in a term word: `beam_message`'s frozen M1 container `[HEADER_BIN_REF, slot, next]`,
//! written by `make_off_heap_container` and read back by `off_heap_slot`. A `sweep` that
//! compacted the list would silently shift every surviving entry's slot and so invalidate
//! each such on-heap reference — the reason `beam_message::OffHeapTable` documents that it
//! never sweeps. [`OffHeap::sweep`] therefore releases dead entries **in place**: a survivor
//! keeps its slot, a released slot resolves to `None` and is never reused.
//!
//! These tests prove that end-to-end, from outside the crate, through the two things a
//! production consumer sees:
//!
//! * a **simulated on-heap off-heap reference** — the `[HEADER_BIN_REF, slot, next]`
//!   container, with the slot in its `val` word, exactly as `beam_message`'s copier writes
//!   it — must still resolve after a middle entry is swept; and
//! * **refc-binary payload accounting** — a released binary's payload is freed exactly once
//!   while a surviving one keeps its slot and payload.
//!
//! Every `unsafe` block below only touches words this test allocated and wrote itself, and
//! carries a `// SAFETY:` rationale.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_alloc::{AllocRef, AllocatorConfig, CarrierAllocator, Heap, RefcBinary};
use beam_gc::prelude::{Mso, OffHeap, OffHeapEntry};
use beam_term::prelude::{boxed_val, make_boxed, Eterm, ERL_BIN_REF_SIZE, HEADER_BIN_REF};

/// A payload that counts how many times it has been dropped.
struct DropCounter {
    drops: Arc<AtomicUsize>,
    value: u32,
}

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// A fresh [`DropCounter`] sharing `drops`.
fn counter(drops: &Arc<AtomicUsize>, value: u32) -> DropCounter {
    DropCounter {
        drops: Arc::clone(drops),
        value,
    }
}

/// An isolated carrier allocator, so a test can observe a binary's payload being freed
/// through `AllocStats` without racing the process-wide registry allocator.
fn test_allocator(name: &str) -> AllocRef {
    CarrierAllocator::new(AllocatorConfig::builder().name(name).build())
        .expect("allocator construction maps nothing")
        .share()
}

/// Writes the frozen `[HEADER_BIN_REF, val, next]` container for `slot` at the top of `heap`
/// and returns its term — exactly the on-heap off-heap reference `beam_message`'s copier
/// writes (`beam_message::layout::make_off_heap_container`).
fn write_slot_container(heap: &mut Heap, slot: usize) -> Eterm {
    // SAFETY: exactly `ERL_BIN_REF_SIZE` (3) exclusively-owned, writable words are reserved.
    let p = unsafe {
        heap.alloc_words(ERL_BIN_REF_SIZE)
            .expect("3 words for the container")
    };
    // SAFETY: `p` is the base of those three words, so all three writes are in bounds.
    unsafe {
        *p = HEADER_BIN_REF;
        *p.add(1) = Eterm::from_raw(slot);
        *p.add(2) = Eterm::from_raw(0);
    }
    make_boxed(p as *const Eterm)
}

/// The off-heap slot stored in the container's `val` word
/// (`beam_message::layout::off_heap_slot`).
fn slot_of(container: Eterm) -> usize {
    // SAFETY: `container` is the live `BinRef` thing `write_slot_container` wrote; word 1 is
    // its `val`.
    unsafe { *boxed_val(container).add(1) }.raw()
}

/// An MSO payload's `value`, read through the entry at `slot`.
fn mso_value(off_heap: &OffHeap, slot: usize) -> Option<u32> {
    off_heap
        .get(slot)
        .and_then(OffHeapEntry::as_mso)
        .and_then(|handle| handle.downcast_ref::<DropCounter>())
        .map(|c| c.value)
}

/// A binary payload's bytes, read through the entry at `slot`.
fn binary_bytes(off_heap: &OffHeap, slot: usize) -> Option<&[u8]> {
    off_heap
        .get(slot)
        .and_then(OffHeapEntry::as_binary)
        .map(RefcBinary::as_bytes)
}

/// **The on-heap off-heap reference survives a middle sweep.** The MSO slots are named by
/// `[HEADER_BIN_REF, slot, next]` containers in a heap; releasing slots 1 and 2 must leave
/// slots 0 and 3 resolving to *their* objects at their *original* slots (a compacting sweep
/// would have shifted slot 3 down to slot 1).
#[test]
fn a_surviving_slot_in_an_on_heap_container_still_resolves_after_a_middle_sweep() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut off_heap = OffHeap::new();

    // Four off-heap MSOs at slots 0..=3, each with a distinct apparent size so the liveness
    // predicate can select a *middle* pair.
    let mut slots = Vec::new();
    for (value, size) in [(0u32, 16usize), (1, 17), (2, 18), (3, 19)] {
        slots.push(off_heap.insert(Mso::new(counter(&drops, value), size)));
    }
    assert_eq!(slots, vec![0, 1, 2, 3]);
    assert_eq!(off_heap.len(), 4);
    assert_eq!(off_heap.overhead_bytes(), 16 + 17 + 18 + 19);

    // The on-heap off-heap references: one container per slot, each naming its slot in `val`.
    let mut heap = Heap::new(64).expect("heap");
    let containers: Vec<Eterm> = slots
        .iter()
        .map(|&s| write_slot_container(&mut heap, s))
        .collect();
    for (i, container) in containers.iter().enumerate() {
        assert_eq!(
            slot_of(*container),
            i,
            "the container's `val` word names its slot"
        );
    }

    // Sweep out the MIDDLE entries (slots 1 and 2) and keep the outer ones.
    let released = off_heap.sweep(|entry| matches!(entry.apparent_size(), 16 | 19));
    assert_eq!(released, 2, "slots 1 and 2 were released");
    assert_eq!(
        drops.load(Ordering::SeqCst),
        2,
        "each released entry's destructor ran exactly once"
    );

    // (a) The surviving containers still resolve — AT THEIR ORIGINAL SLOT — to their original
    // objects. A compacting sweep would have shifted slot 3 down to slot 1.
    assert_eq!(mso_value(&off_heap, 0), Some(0), "slot 0 is unchanged");
    assert_eq!(
        mso_value(&off_heap, 3),
        Some(3),
        "slot 3 must not have moved"
    );
    assert_eq!(
        mso_value(&off_heap, slot_of(containers[3])),
        Some(3),
        "the container written for slot 3 must still name that object"
    );

    // (b) The released slots answer `None` — never an unrelated shifted-in object.
    assert!(
        off_heap.get(1).is_none(),
        "released slot 1 resolves to `None`"
    );
    assert!(
        off_heap.get(2).is_none(),
        "released slot 2 resolves to `None`"
    );
    assert!(off_heap.get(slot_of(containers[1])).is_none());
    assert!(off_heap.get(slot_of(containers[2])).is_none());

    // (c) A further `insert` appends at a fresh index and never reuses a released slot.
    let fresh = off_heap.insert(Mso::new(counter(&drops, 9), 20));
    assert_eq!(fresh, 4, "a released slot is never reused");
    assert_eq!(
        slot_of(write_slot_container(&mut heap, fresh)),
        4,
        "the fresh entry lands at slot 4"
    );
    assert_eq!(mso_value(&off_heap, 4), Some(9));
    assert!(off_heap.get(1).is_none(), "slot 1 stays released");
    assert!(off_heap.get(2).is_none(), "slot 2 stays released");

    // (d) The live-only views count/iterate/sum the survivors alone, in slot order.
    let live_sizes: Vec<usize> = off_heap.iter().map(OffHeapEntry::overhead_bytes).collect();
    assert_eq!(
        live_sizes,
        vec![16, 19, 20],
        "only the survivors, in slot order"
    );
    assert_eq!(off_heap.iter().count(), off_heap.len());
    assert_eq!(off_heap.len(), 3);
    assert_eq!(off_heap.overhead_bytes(), 16 + 19 + 20);

    // (e) The released destructors never run a second time: another retaining sweep releases
    // nothing, and dropping the list drops each remaining entry exactly once.
    assert_eq!(
        off_heap.sweep(|_| true),
        0,
        "a retaining sweep releases nothing"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 2, "no destructor ran again");

    drop(off_heap);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        5,
        "each of the five entries was dropped exactly once, ever"
    );
}

/// **A released binary frees its payload exactly once; a survivor keeps its slot and
/// payload.** The list owns the only reference to each binary, so a release really frees a
/// buffer (counted through the isolated allocator) and a survivor really keeps one.
#[test]
fn a_released_binary_frees_its_payload_once_while_a_survivor_keeps_its_slot() {
    let alloc = test_allocator("offheap-slot-stability-binaries");
    let mut off_heap = OffHeap::new();

    // Three refc binaries at slots 0..=2, payloads of 4, 6 and 2 bytes.
    off_heap.push(RefcBinary::with_allocator(alloc.clone(), b"aaaa").expect("payload 0"));
    off_heap.push(RefcBinary::with_allocator(alloc.clone(), b"bbbbbb").expect("payload 1"));
    off_heap.push(RefcBinary::with_allocator(alloc.clone(), b"cc").expect("payload 2"));
    assert_eq!(off_heap.len(), 3);
    assert_eq!(off_heap.overhead_bytes(), 4 + 6 + 2);
    // The list's bookkeeping is the summed *payload* bytes; the allocator charges its own
    // per-block overhead on top, so only the block count is asserted exactly here.
    assert_eq!(
        alloc.stats().live_blocks,
        3,
        "three payload buffers are live"
    );

    // The on-heap off-heap references, one per slot.
    let mut heap = Heap::new(64).expect("heap");
    let containers: Vec<Eterm> = (0..3).map(|s| write_slot_container(&mut heap, s)).collect();
    assert_eq!(
        containers.iter().map(|c| slot_of(*c)).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );

    // Release the MIDDLE binary (slot 1). Its payload buffer is freed exactly once, and it is
    // not double-freed by any later sweep.
    assert_eq!(off_heap.sweep(|entry| entry.overhead_bytes() != 6), 1);
    assert_eq!(
        alloc.stats().live_blocks,
        2,
        "the released payload's buffer was freed exactly once"
    );
    assert_eq!(off_heap.len(), 2);
    assert_eq!(off_heap.overhead_bytes(), 4 + 2);

    // (a) The surviving binaries are still reachable at their ORIGINAL slots, payloads intact.
    assert_eq!(
        binary_bytes(&off_heap, slot_of(containers[0])),
        Some(&b"aaaa"[..])
    );
    assert_eq!(
        binary_bytes(&off_heap, slot_of(containers[2])),
        Some(&b"cc"[..])
    );
    // (b) The released slot resolves to `None`, never to the surviving binary.
    assert!(off_heap.get(slot_of(containers[1])).is_none());
    assert!(off_heap.get(1).is_none());

    // (c) A further insert appends at a fresh index and never reuses the released slot 1.
    let fresh =
        off_heap.insert(RefcBinary::with_allocator(alloc.clone(), b"dddddddd").expect("payload 3"));
    assert_eq!(fresh, 3, "a released slot is never reused");
    assert!(off_heap.get(1).is_none(), "slot 1 stays released");
    assert_eq!(alloc.stats().live_blocks, 3);

    // A second sweep releases the two older binaries (slots 0 and 2) but never re-touches the
    // already-released slot 1: exactly two more payloads are freed.
    assert_eq!(off_heap.sweep(|entry| entry.overhead_bytes() == 8), 2);
    assert_eq!(
        alloc.stats().live_blocks,
        1,
        "only the fresh binary survives"
    );

    // Dropping the list releases the last payload exactly once — no leak, no double free.
    drop(off_heap);
    assert_eq!(alloc.stats().live_blocks, 0);
    assert_eq!(alloc.stats().live_bytes, 0);
}
